#![no_main]
#![no_std]
#![feature(type_alias_impl_trait)]

use defmt::*;
use defmt_rtt as _;
use panic_probe as _;
use systick_monotonic::*;

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

        normal_task::spawn().unwrap();
        async_task::spawn(5).unwrap();

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
    async fn async_task(_cx: async_task::Context, x: u32) {
        info!("hello from async {}", x);
    }
}
