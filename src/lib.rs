#![no_std]
//! WS2812 PIO Driver for the RP2040
//!
//! This driver implements driving a WS2812 RGB LED strip from
//! a PIO device of the RP2040 chip.
//!
//! You should reach to [Ws2812] if you run the main loop
//! of your controller yourself and you want [Ws2812] to take
//! a hold of your timer.
//!
//! In case you use `cortex-m-rtic` and can't afford this crate
//! to wait blocking for you, you should try [Ws2812Direct].
//! Bear in mind that you will have to take care of timing requirements
//! yourself then.

use embedded_hal::timer::CountDown;
use fugit::{ExtU32, HertzU32, MicrosDurationU32};

use cfg_if::cfg_if;

cfg_if! {
    if #[cfg(feature = "rp2040")] {
        use rp2040_hal as hal;
    } else if #[cfg(feature = "rp235x")] {
        use rp235x_hal as hal;
    } else {
        compile_error!("Either 'rp2040' or 'rp235x' feature must be enabled.");
    }
}

use hal::{
    gpio::AnyPin,
    pio::{PIOExt, StateMachineIndex, Tx, UninitStateMachine, PIO},
};

use smart_leds_trait::SmartLedsWrite;
use smart_leds_trait_0_2::SmartLedsWrite as SmartLedsWrite02;

/// This is the WS2812 PIO Driver.
///
/// For blocking applications is recommended to use
/// the [Ws2812] struct instead of this raw driver.
///
/// If you use this driver directly, you will need to
/// take care of the timing expectations of the [Ws2812Direct::write]
/// method.
///
/// Typical usage example:
///```ignore
/// use hal::clocks::init_clocks_and_plls;
/// let clocks = init_clocks_and_plls(...);
/// let pins = hal::gpio::pin::bank0::Pins::new(...);
///
/// let (mut pio, sm0, _, _, _) = pac.PIO0.split(&mut pac.RESETS);
/// let mut ws = Ws2812Direct::new(
///     pins.gpio4.into_mode(),
///     &mut pio,
///     sm0,
///     clocks.peripheral_clock.freq(),
/// );
///
/// // Then you will make sure yourself to not write too frequently:
/// loop {
///     use smart_leds::{SmartLedsWrite, RGB8};
///     let color : RGB8 = (255, 0, 255).into();
///
///     ws.write([color].iter().copied()).unwrap();
///     delay_for_at_least_60_microseconds();
/// };
///```

pub struct Ws2812Direct<P, SM, I>
where
    I: AnyPin<Function = P::PinFunction>,
    P: PIOExt,
    SM: StateMachineIndex,
{
    tx: Tx<(P, SM)>,
    _pin: I,
}

impl<P, SM, I> Ws2812Direct<P, SM, I>
where
    I: AnyPin<Function = P::PinFunction>,
    P: PIOExt,
    SM: StateMachineIndex,
{
    /// Creates a new instance of this driver.
    pub fn new(
        pin: I,
        pio: &mut PIO<P>,
        sm: UninitStateMachine<(P, SM)>,
        clock_freq: fugit::HertzU32,
    ) -> Self {
        // prepare the PIO program
        let side_set = pio::SideSet::new(false, 1, false);
        let mut a = pio::Assembler::new_with_side_set(side_set);

        const T1: u8 = 2; // start bit
        const T2: u8 = 5; // data bit
        const T3: u8 = 3; // stop bit
        const CYCLES_PER_BIT: u32 = (T1 + T2 + T3) as u32;
        const FREQ: HertzU32 = HertzU32::kHz(800);

        let mut wrap_target = a.label();
        let mut wrap_source = a.label();
        let mut do_zero = a.label();
        a.bind(&mut wrap_target);
        // Do stop bit
        a.out_with_delay_and_side_set(pio::OutDestination::X, 1, T3 - 1, 0);
        // Do start bit
        a.jmp_with_delay_and_side_set(pio::JmpCondition::XIsZero, &mut do_zero, T1 - 1, 1);
        // Do data bit = 1
        a.jmp_with_delay_and_side_set(pio::JmpCondition::Always, &mut wrap_target, T2 - 1, 1);
        a.bind(&mut do_zero);
        // Do data bit = 0
        a.nop_with_delay_and_side_set(T2 - 1, 0);
        a.bind(&mut wrap_source);
        let program = a.assemble_with_wrap(wrap_source, wrap_target);

        // Install the program into PIO instruction memory.
        let installed = pio.install(&program).unwrap();

        // Configure the PIO state machine.
        let bit_freq = FREQ * CYCLES_PER_BIT;
        let mut int = clock_freq / bit_freq;
        let rem = clock_freq - (int * bit_freq);
        let frac = (rem * 256) / bit_freq;
        assert!(
            (1..=65536).contains(&int) && (int != 65536 || frac == 0),
            "(System Clock / {}) must be within [1.0, 65536.0].",
            bit_freq.to_kHz()
        );

        // 65536.0 is represented as 0 in the pio's clock divider
        if int == 65536 {
            int = 0;
        }
        // Using lossy conversion because range have been checked
        let int: u16 = int as u16;
        let frac: u8 = frac as u8;

        let pin = pin.into();
        let (mut sm, _, tx) = hal::pio::PIOBuilder::from_installed_program(installed)
            // only use TX FIFO
            .buffers(hal::pio::Buffers::OnlyTx)
            // Pin configuration
            .side_set_pin_base(pin.id().num)
            // OSR config
            .out_shift_direction(hal::pio::ShiftDirection::Left)
            .autopull(true)
            .pull_threshold(24)
            .clock_divisor_fixed_point(int, frac)
            .build(sm);

        // Prepare pin's direction.
        sm.set_pindirs([(pin.id().num, hal::pio::PinDir::Output)]);

        sm.start();

        Self {
            tx,
            _pin: I::from(pin),
        }
    }
}

impl<P, SM, I> SmartLedsWrite for Ws2812Direct<P, SM, I>
where
    I: AnyPin<Function = P::PinFunction>,
    P: PIOExt,
    SM: StateMachineIndex,
{
    type Color = smart_leds_trait::RGB8;
    type Error = ();
    /// If you call this function, be advised that you will have to wait
    /// at least 60 microseconds between calls of this function!
    /// That means, either you get hold on a timer and the timing
    /// requirements right your self, or rather use [Ws2812].
    ///
    /// Please bear in mind, that it still blocks when writing into the
    /// PIO FIFO until all data has been transmitted to the LED chain.
    fn write<T, J>(&mut self, iterator: T) -> Result<(), ()>
    where
        T: IntoIterator<Item = J>,
        J: Into<Self::Color>,
    {
        for item in iterator {
            let color: Self::Color = item.into();
            let word =
                (u32::from(color.g) << 24) | (u32::from(color.r) << 16) | (u32::from(color.b) << 8);

            while !self.tx.write(word) {
                cortex_m::asm::nop();
            }
        }
        Ok(())
    }
}

impl<P, SM, I> SmartLedsWrite02 for Ws2812Direct<P, SM, I>
where
    I: AnyPin<Function = P::PinFunction>,
    P: PIOExt,
    SM: StateMachineIndex,
{
    type Color = smart_leds_trait::RGB8;
    type Error = ();
    /// If you call this function, be advised that you will have to wait
    /// at least 60 microseconds between calls of this function!
    /// That means, either you get hold on a timer and the timing
    /// requirements right your self, or rather use [Ws2812].
    ///
    /// Please bear in mind, that it still blocks when writing into the
    /// PIO FIFO until all data has been transmitted to the LED chain.
    fn write<T, J>(&mut self, iterator: T) -> Result<(), ()>
    where
        T: Iterator<Item = J>,
        J: Into<Self::Color>,
    {
        SmartLedsWrite::write(self, iterator)
    }
}

/// Instance of a WS2812 LED chain.
///
/// Use the [Ws2812::write] method to update the WS2812 LED chain.
///
/// Typical usage example:
///```ignore
/// use hal::clocks::init_clocks_and_plls;
/// let clocks = init_clocks_and_plls(...);
/// let pins = hal::gpio::pin::bank0::Pins::new(...);
///
/// let timer = Timer::new(pac.TIMER, &mut pac.RESETS);
///
/// let (mut pio, sm0, _, _, _) = pac.PIO0.split(&mut pac.RESETS);
/// let mut ws = Ws2812::new(
///     pins.gpio4.into_mode(),
///     &mut pio,
///     sm0,
///     clocks.peripheral_clock.freq(),
///     timer.count_down(),
/// );
///
/// loop {
///     use smart_leds::{SmartLedsWrite, RGB8};
///     let color : RGB8 = (255, 0, 255).into();
///
///     ws.write([color].iter().copied()).unwrap();
///
///     // Do other stuff here...
/// };
///```

pub struct Ws2812<P, SM, C, I>
where
    C: CountDown,
    I: AnyPin<Function = P::PinFunction>,
    P: PIOExt,
    SM: StateMachineIndex,
{
    driver: Ws2812Direct<P, SM, I>,
    cd: C,
}

impl<P, SM, C, I> Ws2812<P, SM, C, I>
where
    C: CountDown,
    I: AnyPin<Function = P::PinFunction>,
    P: PIOExt,
    SM: StateMachineIndex,
{
    /// Creates a new instance of this driver.
    pub fn new(
        pin: I,
        pio: &mut PIO<P>,
        sm: UninitStateMachine<(P, SM)>,
        clock_freq: fugit::HertzU32,
        cd: C,
    ) -> Ws2812<P, SM, C, I> {
        let driver = Ws2812Direct::new(pin, pio, sm, clock_freq);

        Self { driver, cd }
    }
}
impl<P, SM, I, C> SmartLedsWrite for Ws2812<P, SM, C, I>
where
    C: CountDown,
    C::Time: From<MicrosDurationU32>,
    I: AnyPin<Function = P::PinFunction>,
    P: PIOExt,
    SM: StateMachineIndex,
{
    type Color = smart_leds_trait::RGB8;
    type Error = ();
    fn write<T, J>(&mut self, iterator: T) -> Result<(), ()>
    where
        T: IntoIterator<Item = J>,
        J: Into<Self::Color>,
    {
        self.driver.tx.clear_stalled_flag();
        while !self.driver.tx.is_empty() && !self.driver.tx.has_stalled() {}

        self.cd.start(60u32.micros());
        let _ = nb::block!(self.cd.wait());

        SmartLedsWrite::write(&mut self.driver, iterator)
    }
}
impl<P, SM, I, C> SmartLedsWrite02 for Ws2812<P, SM, C, I>
where
    C: CountDown,
    C::Time: From<MicrosDurationU32>,
    I: AnyPin<Function = P::PinFunction>,
    P: PIOExt,
    SM: StateMachineIndex,
{
    type Color = smart_leds_trait::RGB8;
    type Error = ();
    fn write<T, J>(&mut self, iterator: T) -> Result<(), ()>
    where
        T: IntoIterator<Item = J>,
        J: Into<Self::Color>,
    {
        SmartLedsWrite::write(self, iterator)
    }
}
