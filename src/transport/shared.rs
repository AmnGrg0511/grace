//! An indirection over the provider transport so the agent loop can swap
//! transports mid-session ([`SharedTransport::swap`]) while every consumer
//! keeps working on the exact transport it started with.

use std::cell::RefCell;
use std::rc::Rc;

use crate::transport::ProviderTransport;

/// A transport slot the interactive session owns. `/model` and `/provider`
/// swap in a replacement at the prompt; the turn runner, status line, and
/// tools acquire a *clone* of the current transport via [`SharedTransport::current`]
/// and hold it for as long as they need it.
///
/// Swaps only happen between turns, so a clone taken at the start of a turn is
/// always the transport that turn was invoked against. A `Box<dyn ProviderTransport>`
/// widened to `Rc<dyn ProviderTransport>` keeps every clone aliasing the same
/// concrete transport, so a delegate tool re-acquiring `current()` mid-turn
/// still sees the same provider, model, and credentials as its parent.
#[derive(Clone)]
pub struct SharedTransport(Rc<RefCell<Rc<dyn ProviderTransport>>>);

impl SharedTransport {
    /// Wrap the initial transport.
    pub fn new(transport: Box<dyn ProviderTransport>) -> Self {
        Self(Rc::new(RefCell::new(Rc::from(transport))))
    }

    /// A clone of the current transport. The caller can hold it across a
    /// later [`SharedTransport::swap`] without it being invalidated.
    pub fn current(&self) -> Rc<dyn ProviderTransport> {
        self.0.borrow().clone()
    }

    /// Atomically replace the current transport. The replaced transport stays
    /// alive until every outstanding [`SharedTransport::current`] clone is
    /// dropped, so a swap between turns never breaks a turn still finishing
    /// on the old one.
    pub fn swap(&self, transport: Box<dyn ProviderTransport>) {
        *self.0.borrow_mut() = Rc::from(transport);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{copilot, transport_for};

    #[test]
    fn current_returns_the_transport_that_was_swapped_in() {
        let shared =
            SharedTransport::new(transport_for("https://api.openai.com/v1", "k", "m", None));
        assert_eq!(shared.current().name(), "openai-http");

        shared.swap(transport_for(copilot::BASE_URL, "t", "m", None));
        assert_eq!(shared.current().name(), "github-copilot");
    }

    #[test]
    fn clones_of_the_slot_share_it() {
        let shared =
            SharedTransport::new(transport_for("https://api.openai.com/v1", "k", "m", None));
        let other = shared.clone();
        assert_eq!(other.current().name(), "openai-http");

        shared.swap(transport_for(copilot::BASE_URL, "t", "m", None));
        assert_eq!(other.current().name(), "github-copilot");
    }

    #[test]
    fn a_current_clone_is_not_invalidated_by_a_swap() {
        let shared =
            SharedTransport::new(transport_for("https://api.openai.com/v1", "k", "m", None));
        let in_flight = shared.current();
        shared.swap(transport_for(copilot::BASE_URL, "t", "m", None));

        // The turn that took `in_flight` before the swap keeps its transport;
        // only the next `current()` sees the swap.
        assert_eq!(in_flight.name(), "openai-http");
        assert_eq!(shared.current().name(), "github-copilot");
    }
}