//! Embassy BLE Example
//!
//! - starts Bluetooth advertising
//! - offers one service with three characteristics (one is read/write, one is write only, one is
//!   read/write/notify)
//! - pressing the boot-button on a dev-board will send a notification if it is subscribed

//% FEATURES: embassy esp-radio esp-radio/ble esp-hal/unstable
//% CHIPS: esp32 esp32s3 esp32c2 esp32c3 esp32c6 esp32h2

// Embassy offers another compatible BLE crate [trouble](https://github.com/embassy-rs/trouble/tree/main/examples/esp32) with esp32 examples.

#![no_std]
#![no_main]

use core::cell::RefCell;

use bleps::{
    ad_structure::{
        AdStructure,
        BR_EDR_NOT_SUPPORTED,
        LE_GENERAL_DISCOVERABLE,
        create_advertising_data,
    },
    async_attribute_server::AttributeServer,
    asynch::Ble,
    attribute_server::NotificationData,
    gatt,
};
use embassy_executor::Spawner;
use embassy_futures::{select::{Either, select}};
use embassy_time::Timer;
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Pull},
    timer::timg::TimerGroup,
    time
};
use esp_println::println;
use esp_radio::{Controller, ble::controller::BleConnector};
use bleps::{PollResult, event::EventType, att::Uuid};

esp_bootloader_esp_idf::esp_app_desc!();

// When you are okay with using a nightly compiler it's better to use https://docs.rs/static_cell/2.1.0/static_cell/macro.make_static.html
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    let rtc = esp_hal::rtc_cntl::Rtc::new(peripherals.LPWR);
    esp_alloc::heap_allocator!(size: 72 * 1024);

    // Initialize esp-radio preemption system BEFORE esp_radio::init()
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_radio_preempt_baremetal::init(timg0.timer0);

    let esp_wifi_ctrl = &*mk_static!(Controller<'static>, esp_radio::init().unwrap());

    let config = InputConfig::default().with_pull(Pull::Down);
    cfg_if::cfg_if! {
        if #[cfg(any(feature = "esp32", feature = "esp32s2", feature = "esp32s3"))] {
            let button = Input::new(peripherals.GPIO0, config);
        } else {
            let button = Input::new(peripherals.GPIO9, config);
        }
    }

    cfg_if::cfg_if! {
        if #[cfg(feature = "esp32")] {
            let timg1 = TimerGroup::new(peripherals.TIMG1);
            esp_hal_embassy::init(timg1.timer0);
        } else {
            use esp_hal::timer::systimer::SystemTimer;
            let systimer = SystemTimer::new(peripherals.SYSTIMER);
            esp_hal_embassy::init(systimer.alarm0);
        }
    }

    spawner.spawn(ble_task(esp_wifi_ctrl, peripherals.BT, rtc, button)).ok();

    loop {
        embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
        println!("Waiting for button press...");
    }
}

#[embassy_executor::task]
async fn ble_task(
    esp_wifi_ctrl: &'static Controller<'static>,
    bt: esp_hal::peripherals::BT<'static>,
    mut rtc: esp_hal::rtc_cntl::Rtc<'static>,
    button: Input<'static>
) {
    let mut sleep_config = esp_hal::rtc_cntl::sleep::RtcSleepConfig::default();
    sleep_config.set_modem_pd_en(false);
    sleep_config.set_int_8m_pd_en(false);
    sleep_config.set_rtc_peri_pd_en(false);
    sleep_config.set_dig_peri_pd_en(false);
    sleep_config.set_rtc_fastmem_pd_en(false);
    sleep_config.set_rtc_slowmem_pd_en(false);
    sleep_config.set_rtc_regulator_fpu(true);
    sleep_config.set_xtal_fpu(true);
    sleep_config.set_rtc_mem_inf_follow_cpu(true);
    sleep_config.set_cpu_pd_en(false);
    sleep_config.set_deep_slp(false);
    sleep_config.set_deep_slp_reject(false);
    sleep_config.set_light_slp_reject(false);
    let timer = esp_hal::rtc_cntl::sleep::TimerWakeupSource::new(core::time::Duration::from_secs(5));

    let pin_ref = RefCell::new(button);
    let pin_ref = &pin_ref;

    // Create connector once (bt can only be moved once)
    let mut connector = BleConnector::new(esp_wifi_ctrl, bt);
    
    loop {
        connector.reinit();
        println!("Reinitialized BLE controller");
        // Recreate BLE stack for each sleep/wakeup cycle - best approximation of full power recycle
        let now = || time::Instant::now().duration_since_epoch().as_millis();
        let mut ble = Ble::new(&mut connector, now);
        
        // Re-initialize BLE stack state for advertising and GATT server
        println!("{:?}", ble.init().await);
        println!("{:?}", ble.cmd_set_le_advertising_parameters().await);
        println!(
            "{:?}",
            ble.cmd_set_le_advertising_data(
                create_advertising_data(&[
                    AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
                    AdStructure::ServiceUuids16(&[Uuid::Uuid16(0x1809)]),
                    AdStructure::CompleteLocalName("esp32s3_tu"),
                ])
                .unwrap()
            )
            .await
        );

        println!("{:?}", ble.cmd_set_le_advertise_enable(true).await);
        println!("started advertising");
        let start_time = embassy_time::Instant::now();
        loop {
            match select(ble.poll(), Timer::after_millis(50)).await {
                Either::First(status) => {
                    if let Some(PollResult::Event(EventType::ConnectionComplete{ status: _, handle: _, role: _, peer_address: _, interval: _, latency: _, timeout: _ })) = status {
                        let mut rf = |_offset: usize, data: &mut [u8]| {
                            data[..20].copy_from_slice(&b"Hello Bare-Metal BLE"[..]);
                            17
                        };
                        let mut wf = |offset: usize, data: &[u8]| {
                            println!("RECEIVED: {} {:?}", offset, data);
                        };

                        let mut wf2 = |offset: usize, data: &[u8]| {
                            println!("RECEIVED: {} {:?}", offset, data);
                        };

                        let mut rf3 = |_offset: usize, data: &mut [u8]| {
                            data[..5].copy_from_slice(&b"Hola!"[..]);
                            5
                        };
                        let mut wf3 = |offset: usize, data: &[u8]| {
                            println!("RECEIVED: Offset {}, data {:?}", offset, data);
                        };

                        gatt!([service {
                            uuid: "937312e0-2354-11eb-9f10-fbc30a62cf38",
                            characteristics: [
                                characteristic {
                                    uuid: "937312e0-2354-11eb-9f10-fbc30a62cf38",
                                    read: rf,
                                    write: wf,
                                },
                                characteristic {
                                    uuid: "957312e0-2354-11eb-9f10-fbc30a62cf38",
                                    write: wf2,
                                },
                                characteristic {
                                    name: "my_characteristic",
                                    uuid: "987312e0-2354-11eb-9f10-fbc30a62cf38",
                                    notify: true,
                                    read: rf3,
                                    write: wf3,
                                },
                            ],
                        },]);

                        let mut rng = bleps::no_rng::NoRng;
                        let mut srv = AttributeServer::new(&mut ble, &mut gatt_attributes, &mut rng);

                        let counter = RefCell::new(0u8);
                        let counter = &counter;

                        let mut notifier = || {
                            // TODO how to check if notifications are enabled for the characteristic?
                            // maybe pass something into the closure which just can query the characteristic
                            // value probably passing in the attribute server won't work?

                            async {
                                pin_ref.borrow_mut().wait_for_rising_edge().await;
                                let mut data = [0u8; 13];
                                data.copy_from_slice(b"Notification0");
                                {
                                    let mut counter = counter.borrow_mut();
                                    data[data.len() - 1] += *counter;
                                    *counter = (*counter + 1) % 10;
                                }
                                NotificationData::new(my_characteristic_handle, &data)
                            }
                        };

                        srv.run(&mut notifier).await.unwrap();
                        break;
                    }
                }
                Either::Second(_) => {
                    // nothing to do
                }
            }

                if start_time.elapsed().as_secs() > 10 {
                break;
            }
        }
        println!("Enter sleep mode");
        println!("{:?}", ble.cmd_set_le_advertise_enable(false).await);
        println!("BLE advertising stopped, entering sleep...");
        embassy_time::Timer::after(embassy_time::Duration::from_millis(100)).await;
        rtc.sleep(&sleep_config, &[&timer]);
        embassy_time::Timer::after(embassy_time::Duration::from_millis(100)).await;
        println!("wakeup! Starting next BLE cycle...");
    }
}