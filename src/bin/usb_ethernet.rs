#![no_main]
#![no_std]
#![feature(type_alias_impl_trait)]

use core::mem;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::Waker;

use defmt::*;
use embassy::blocking_mutex::raw::ThreadModeRawMutex;
use embassy::channel::mpmc::Channel;
use embassy::util::Forever;
use embassy_net::tcp::TcpSocket;
use embassy_net::{PacketBox, PacketBoxExt, PacketBuf, Stack, StackResources};
use embassy_nrf::rng::Rng;
use embassy_nrf::usb::Driver;
use embassy_nrf::{interrupt, pac, peripherals, Peripherals};
use embassy_usb::{Builder, Config, UsbDevice};
use embassy_usb_ncm::{CdcNcmClass, Receiver, Sender, State};
use embedded_io::asynch::{Read, Write};
use panic_probe as _;
use systick_monotonic::*;
use {defmt_rtt as _, panic_probe as _};

type MyDriver = Driver<'static, peripherals::USBD>;

macro_rules! forever {
    ($val:expr) => {{
        type T = impl Sized;
        static FOREVER: Forever<T> = Forever::new();
        FOREVER.put_with(move || $val)
    }};
}

// NOTES:
//
// - Async tasks cannot have `#[lock_free]` resources, as they can interleve and each async
//   task can have a mutable reference stored.
// - Spawning an async task equates to it being polled at least once.
// - ...

#[rtic::app(device = embassy_nrf::pac, dispatchers = [SWI0_EGU0, SWI1_EGU1], peripherals = true)]
mod app {
    use crate::*;

    pub type AppInstant = <Systick<100> as rtic::Monotonic>::Instant;
    pub type AppDuration = <Systick<100> as rtic::Monotonic>::Duration;

    #[shared]
    struct Shared {}

    #[local]
    struct Local {}

    #[monotonic(binds = SysTick, default = true)]
    type MyMono = Systick<100>;

    #[init]
    fn init(cx: init::Context) -> (Shared, Local, init::Monotonics) {
        info!("init");

        let p = embassy_nrf::init(Default::default());

        normal_task::spawn().ok();
        async_task::spawn(p).ok();

        (
            Shared {},
            Local {},
            init::Monotonics(Systick::new(cx.core.SYST, 12_000_000)),
        )
    }

    #[idle]
    fn idle(_: idle::Context) -> ! {
        // debug::exit(debug::EXIT_SUCCESS);
        loop {
            // hprintln!("idle");
            cortex_m::asm::wfi(); // put the MCU in sleep mode until interrupt occurs
        }
    }

    #[task]
    fn normal_task(_cx: normal_task::Context) {
        info!("hello from normal");
    }

    #[task]
    async fn async_task(_cx: async_task::Context, p: Peripherals) {
        info!("hello from async");

        let clock: pac::CLOCK = unsafe { mem::transmute(()) };
        let power: pac::POWER = unsafe { mem::transmute(()) };

        info!("Enabling ext hfosc...");
        clock.tasks_hfclkstart.write(|w| unsafe { w.bits(1) });
        while clock.events_hfclkstarted.read().bits() != 1 {}

        info!("Waiting for vbus...");
        while !power.usbregstatus.read().vbusdetect().is_vbus_present() {}
        info!("vbus OK");

        // Create the driver, from the HAL.
        let irq = interrupt::take!(USBD);
        let driver = Driver::new(p.USBD, irq);

        // Create embassy-usb Config
        let mut config = Config::new(0xc0de, 0xcafe);
        config.manufacturer = Some("Embassy");
        config.product = Some("USB-Ethernet example");
        config.serial_number = Some("12345678");
        config.max_power = 100;
        config.max_packet_size_0 = 64;

        // Required for Windows support.
        config.composite_with_iads = true;
        config.device_class = 0xEF;
        config.device_sub_class = 0x02;
        config.device_protocol = 0x01;

        struct Resources {
            device_descriptor: [u8; 256],
            config_descriptor: [u8; 256],
            bos_descriptor: [u8; 256],
            control_buf: [u8; 128],
            serial_state: State<'static>,
        }
        let res: &mut Resources = forever!(Resources {
            device_descriptor: [0; 256],
            config_descriptor: [0; 256],
            bos_descriptor: [0; 256],
            control_buf: [0; 128],
            serial_state: State::new(),
        });

        // Create embassy-usb DeviceBuilder using the driver and config.
        let mut builder = Builder::new(
            driver,
            config,
            &mut res.device_descriptor,
            &mut res.config_descriptor,
            &mut res.bos_descriptor,
            &mut res.control_buf,
            None,
        );

        // WARNINGS for Android ethernet tethering:
        // - On Pixel 4a, it refused to work on Android 11, worked on Android 12.
        // - if the host's MAC address has the "locally-administered" bit set (bit 1 of first byte),
        //   it doesn't work! The "Ethernet tethering" option in settings doesn't get enabled.
        //   This is due to regex spaghetti: https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-mainline-12.0.0_r84/core/res/res/values/config.xml#417
        //   and this nonsense in the linux kernel: https://github.com/torvalds/linux/blob/c00c5e1d157bec0ef0b0b59aa5482eb8dc7e8e49/drivers/net/usb/usbnet.c#L1751-L1757

        // Our MAC addr.
        let our_mac_addr = [0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC];
        // Host's MAC addr. This is the MAC the host "thinks" its USB-to-ethernet adapter has.
        let host_mac_addr = [0x88, 0x88, 0x88, 0x88, 0x88, 0x88];

        // Create classes on the builder.
        let class = CdcNcmClass::new(&mut builder, &mut res.serial_state, host_mac_addr, 64);

        // Build the builder.
        let usb = builder.build();

        usb_task::spawn(usb).map_err(|_| ()).unwrap();

        let (tx, rx) = class.split();
        usb_ncm_rx_task::spawn(rx).map_err(|_| ()).unwrap();
        usb_ncm_tx_task::spawn(tx).map_err(|_| ()).unwrap();

        let config = embassy_net::ConfigStrategy::Dhcp;
        //let config = embassy_net::ConfigStrategy::Static(embassy_net::Config {
        //    address: Ipv4Cidr::new(Ipv4Address::new(10, 42, 0, 61), 24),
        //    dns_servers: Vec::new(),
        //    gateway: Some(Ipv4Address::new(10, 42, 0, 1)),
        //});

        // Generate random seed
        let mut rng = Rng::new(p.RNG, interrupt::take!(RNG));
        let mut seed = [0; 8];
        rng.blocking_fill_bytes(&mut seed);
        let seed = u64::from_le_bytes(seed);

        // Init network stack
        let device = Device {
            mac_addr: our_mac_addr,
        };
        let stack = &*forever!(Stack::new(
            device,
            config,
            forever!(StackResources::<1, 2, 8>::new()),
            seed
        ));

        net_task::spawn(stack).map_err(|_| ()).unwrap();

        // And now we can use it!

        let mut rx_buffer = [0; 4096];
        let mut tx_buffer = [0; 4096];
        let mut buf = [0; 4096];

        loop {
            let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
            socket.set_timeout(Some(embassy_net::SmolDuration::from_secs(10)));

            info!("Listening on TCP:1234...");
            if let Err(e) = socket.accept(1234).await {
                warn!("accept error: {:?}", e);
                continue;
            }

            info!("Received connection from {:?}", socket.remote_endpoint());

            loop {
                let n = match socket.read(&mut buf).await {
                    Ok(0) => {
                        warn!("read EOF");
                        break;
                    }
                    Ok(n) => n,
                    Err(e) => {
                        warn!("read error: {:?}", e);
                        break;
                    }
                };

                info!("rxd {:02x}", &buf[..n]);

                match socket.write_all(&buf[..n]).await {
                    Ok(()) => {}
                    Err(e) => {
                        warn!("write error: {:?}", e);
                        break;
                    }
                };
            }
        }
    }

    #[task]
    async fn usb_task(_cx: usb_task::Context, mut device: UsbDevice<'static, MyDriver>) {
        device.run().await
    }

    #[task]
    async fn usb_ncm_rx_task(
        _cx: usb_ncm_rx_task::Context,
        mut class: Receiver<'static, MyDriver>,
    ) {
        loop {
            warn!("WAITING for connection");
            LINK_UP.store(false, Ordering::Relaxed);

            class.wait_connection().await.unwrap();

            warn!("Connected");
            LINK_UP.store(true, Ordering::Relaxed);

            loop {
                let mut p = unwrap!(PacketBox::new(embassy_net::Packet::new()));
                let n = match class.read_packet(&mut p[..]).await {
                    Ok(n) => n,
                    Err(e) => {
                        warn!("error reading packet: {:?}", e);
                        break;
                    }
                };

                let buf = p.slice(0..n);
                if RX_CHANNEL.try_send(buf).is_err() {
                    warn!("Failed pushing rx'd packet to channel.");
                }
            }
        }
    }

    #[task]
    async fn usb_ncm_tx_task(_cx: usb_ncm_tx_task::Context, mut class: Sender<'static, MyDriver>) {
        loop {
            let pkt = TX_CHANNEL.recv().await;
            if let Err(e) = class.write_packet(&pkt[..]).await {
                warn!("Failed to TX packet: {:?}", e);
            }
        }
    }

    #[task]
    async fn net_task(_cx: net_task::Context, stack: &'static Stack<Device>) {
        stack.run().await
    }
}

static TX_CHANNEL: Channel<ThreadModeRawMutex, PacketBuf, 8> = Channel::new();
static RX_CHANNEL: Channel<ThreadModeRawMutex, PacketBuf, 8> = Channel::new();
static LINK_UP: AtomicBool = AtomicBool::new(false);

struct Device {
    mac_addr: [u8; 6],
}

impl embassy_net::Device for Device {
    fn register_waker(&mut self, waker: &Waker) {
        // loopy loopy wakey wakey
        waker.wake_by_ref()
    }

    fn link_state(&mut self) -> embassy_net::LinkState {
        match LINK_UP.load(Ordering::Relaxed) {
            true => embassy_net::LinkState::Up,
            false => embassy_net::LinkState::Down,
        }
    }

    fn capabilities(&self) -> embassy_net::DeviceCapabilities {
        let mut caps = embassy_net::DeviceCapabilities::default();
        caps.max_transmission_unit = 1514; // 1500 IP + 14 ethernet header
        caps.medium = embassy_net::Medium::Ethernet;
        caps
    }

    fn is_transmit_ready(&mut self) -> bool {
        true
    }

    fn transmit(&mut self, pkt: PacketBuf) {
        if TX_CHANNEL.try_send(pkt).is_err() {
            warn!("TX failed")
        }
    }

    fn receive<'a>(&mut self) -> Option<PacketBuf> {
        RX_CHANNEL.try_recv().ok()
    }

    fn ethernet_address(&self) -> [u8; 6] {
        self.mac_addr
    }
}
