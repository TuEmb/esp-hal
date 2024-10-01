//! Embassy access point
//!
//! - creates an open access-point with SSID `esp-wifi`
//! - you can connect to it using a static IP in range 192.168.2.2 .. 192.168.2.255, gateway 192.168.2.1
//! - open http://192.168.2.1:8080/ in your browser - the example will perform an HTTP get request to some "random" server
//!
//! On Android you might need to choose _Keep Accesspoint_ when it tells you the WiFi has no internet connection, Chrome might not want to load the URL - you can use a shell and try `curl` and `ping`
//!
//! Because of the huge task-arena size configured this won't work on ESP32-S2

//% FEATURES: embassy embassy-generic-timers esp-wifi esp-wifi/async esp-wifi/embassy-net esp-wifi/wifi-default esp-wifi/wifi esp-wifi/utils
//% CHIPS: esp32 esp32s2 esp32s3 esp32c2 esp32c3 esp32c6

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_net::{
    tcp::TcpSocket,
    IpListenEndpoint,
    Ipv4Address,
    Ipv4Cidr,
    Stack,
    StackResources,
    StaticConfigV4,
};
use embassy_time::{Duration, Timer};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel::Channel};
use embedded_can::{Frame, Id};
use embedded_storage::Storage;
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{prelude::*, gpio::Io, rng::Rng, timer::timg::TimerGroup,
    peripherals::TWAI0,
    reset::software_reset,
    twai::{self, Twai, TwaiMode},};
use esp_println::println;
use esp_wifi::{
    initialize,
    wifi::{
        AccessPointConfiguration,
        Configuration,
        WifiApDevice,
        WifiController,
        WifiDevice,
        WifiEvent,
        WifiState,
        Protocol,
        AuthMethod,
    },
    EspWifiInitFor,
};
use static_cell::StaticCell;
use esp_storage::FlashStorage;

#[derive(Debug)]
#[allow(dead_code)]
struct CanFrame {
    id: u32,
    data: [u8; 8],
}

type TwaiOutbox = Channel<NoopRawMutex, CanFrame, 16>;

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
    let peripherals = esp_hal::init({
        let mut config = esp_hal::Config::default();
        config.cpu_clock = CpuClock::max();
        config
    });

    esp_alloc::heap_allocator!(128 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);

    let init = initialize(
        EspWifiInitFor::Wifi,
        timg0.timer0,
        Rng::new(peripherals.RNG),
        peripherals.RADIO_CLK,
    )
    .unwrap();

    let io = Io::new(peripherals.GPIO, peripherals.IO_MUX);

    let tx_pin = io.pins.gpio37;
    let rx_pin = io.pins.gpio38;
    const CAN_BAUDRATE: twai::BaudRate = twai::BaudRate::B250K;
    let mut twai_config = twai::TwaiConfiguration::new_async(
        peripherals.TWAI0,
        rx_pin,
        tx_pin,
        CAN_BAUDRATE,
        TwaiMode::Normal,
    );
    twai_config.set_filter(
        const { twai::filter::SingleStandardFilter::new(b"xxxxxxxxxxx", b"x", [b"xxxxxxxx", b"xxxxxxxx"]) },
    );
    let can = twai_config.start();
    static CHANNEL: StaticCell<TwaiOutbox> = StaticCell::new();
    let channel = &*CHANNEL.init(Channel::new());
    spawner.spawn(receiver(can, channel)).ok();

    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(&init, wifi, WifiApDevice).unwrap();

    cfg_if::cfg_if! {
        if #[cfg(feature = "esp32")] {
            let timg1 = TimerGroup::new(peripherals.TIMG1);
            esp_hal_embassy::init(timg1.timer0);
        } else {
            use esp_hal::timer::systimer::{SystemTimer, Target};
            let systimer = SystemTimer::new(peripherals.SYSTIMER).split::<Target>();
            esp_hal_embassy::init(systimer.alarm0);
        }
    }

    let config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Address::new(192, 168, 2, 1), 24),
        gateway: Some(Ipv4Address::from_bytes(&[192, 168, 2, 1])),
        dns_servers: Default::default(),
    });

    let seed = 1234; // very random, very secure seed

    // Init network stack
    let stack = &*mk_static!(
        Stack<WifiDevice<'_, WifiApDevice>>,
        Stack::new(
            wifi_interface,
            config,
            mk_static!(StackResources<3>, StackResources::<3>::new()),
            seed
        )
    );

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(&stack)).ok();

    let mut rx_buffer = [0; 1536];
    let mut tx_buffer = [0; 1536];

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }
    println!("Connect to the AP `esp-wifi` and point your browser to http://192.168.2.1:8080/");
    println!("Use a static IP in the range 192.168.2.2 .. 192.168.2.255, use gateway 192.168.2.1");

    let mut socket = TcpSocket::new(&stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));
    loop {
        println!("Wait for connection...\r");
        let r = socket
            .accept(IpListenEndpoint {
                addr: None,
                port: 8080,
            })
            .await;
        println!("Connected...\r");
        let (mut socket_rx, mut socket_tx) = socket.split();
        if let Err(e) = r {
            println!("connect error: {:?}\r", e);
            continue;
        }
        let mut buffer = [0u8; 1024];
        if let Ok(size) = socket_rx.read(&mut buffer).await {
            if size == 1 && buffer[0] == 0xFA {
                /* Stop diag session */
                let mut flash = FlashStorage::new();
                let mut bytes = [0u8; 32];

                let flash_addr = 0xd000;
                for byte in &mut bytes {
                    *byte = 0xFF;
                }
                flash.write(flash_addr, &bytes).unwrap();
                println!("Written to {:x}: {:02x?}\r", flash_addr, &bytes[..32]);
                software_reset();
            } else {
                println!("socket receive: {:?}\r", buffer);
            }
        } else {
            continue;
        }

        use embedded_io_async::Write;
        loop {
            let frame = channel.receive().await;
            use core::fmt::Write;
            let mut frame_str: heapless::String<80> = heapless::String::new();
            writeln!(&mut frame_str, "{:?}", frame).unwrap();
            match socket_tx.write_all(frame_str.as_bytes()).await {
                Ok(_) => {
                    // println!("Socket sends frame OK\r");
                }
                Err(e) => {
                    println!("Failed to send frame through socket: {:?}\r", e);
                    break;
                }
            }
        }

        Timer::after(Duration::from_millis(1000)).await;
        println!("close the socket !\r");
        socket.close();
        Timer::after(Duration::from_millis(1000)).await;
        println!("abort the socket !\r");
        socket.abort();
    }
}

#[embassy_executor::task]
async fn receiver(
    mut rx: Twai<'static, TWAI0, esp_hal::Async>,
    channel: &'static TwaiOutbox,
) -> ! {
    loop {
        let frame = rx.receive_async().await;

        match frame {
            Ok(frame) => {
                // repeat the frame back
                match frame.id() {
                    // ION doesn't work with StandardId
                    Id::Standard(_id) => {}
                    Id::Extended(id) => {
                        let mut data = [0u8; 8];
                        data[0..frame.data().len()].copy_from_slice(frame.data());
                        // print_frame(&frame)
                        channel
                            .send(CanFrame {
                                id: id.as_raw(),
                                data,
                            })
                            .await;
                    }
                }
            }
            Err(e) => {
                println!("Receive error: {:?}\r", e);
            }
        }
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task\r");
    println!("Device capabilities: {:?}\r", controller.get_capabilities());
    loop {
        if esp_wifi::wifi::get_wifi_state() == WifiState::ApStarted {
            // wait until we're no longer connected
            controller.wait_for_event(WifiEvent::ApStop).await;
            Timer::after(Duration::from_millis(5000)).await
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::AccessPoint(AccessPointConfiguration {
                ssid: "ion-esp-diag".try_into().unwrap(),
                ssid_hidden: false,
                channel: 1,
                secondary_channel: None,
                protocols: Protocol::P802D11B | Protocol::P802D11BG | Protocol::P802D11BGN,
                auth_method: AuthMethod::WPA2Personal,
                password: "p@ssw0rd".try_into().unwrap(),
                max_connections: 5,
            });
            controller.set_configuration(&client_config).unwrap();
            println!("Starting wifi\r");
            controller.start().await.unwrap();
            println!("Wifi started!\r");
        }
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice<'static, WifiApDevice>>) {
    stack.run().await
}
