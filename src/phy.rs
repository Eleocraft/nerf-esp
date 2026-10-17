use core::task::{Context, Poll};

use embassy_time::{Duration, Timer};
use esp_hal::ethernet::{
    mac::{Duplex, LinkState, Speed},
    phy::{MdioBus, Phy, PhyError, generic::GenericPhy},
};
use futures_util::FutureExt;

/// A custom PHY implementation that wraps [`GenericPhy`], and uses embassy-time
/// to implement polling instead of the busy looping done by GenericPhy.
/// (Taken from esp-hal/examples)
pub struct AsyncPhy {
    phy: GenericPhy,
    timer: Timer,
    cached_link_state: LinkState,
}

impl AsyncPhy {
    pub fn new_auto() -> Self {
        Self {
            phy: GenericPhy::new_auto(),
            timer: Timer::after_ticks(0),
            cached_link_state: LinkState {
                up: false,
                speed: Speed::_100M,
                duplex: Duplex::Full,
            },
        }
    }
}

impl Phy for AsyncPhy {
    fn address(&self) -> u8 {
        self.phy.address()
    }

    fn init<M: MdioBus>(&mut self, mdio: &mut M) -> Result<(), PhyError> {
        self.phy.init(mdio)
    }

    fn poll_link<M: MdioBus>(&mut self, mdio: &mut M, cx: Option<&mut Context<'_>>) -> LinkState {
        if let Some(cx) = cx {
            if matches!(self.timer.poll_unpin(cx), Poll::Pending) {
                return self.cached_link_state;
            }

            // Timer done, re-arm it
            self.timer = Timer::after(Duration::from_millis(500));

            // Ensure the next wakeup is scheduled
            let _ = self.timer.poll_unpin(cx);
        }

        // Poll the PHY for link state. Do not pass `cx`, we're handling the scheduling.
        self.cached_link_state = self.phy.poll_link(mdio, None);

        // Uncomment to observe how often the link state is polled
        // println!("Polled link state: {:?}", self.cached_link_state);

        self.cached_link_state
    }
}
