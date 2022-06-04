// MIT/Apache2 License

//! Implements the "special events" pattern for [`breadx`].
//!
//! Some extensions to X11 mandate a pattern known as "special events", where
//! certain events are sorted into queues for libraries to process. It's only
//! ever used in a couple of extensions, and checking for them is not idiomatic
//! to do on the hot path for event processing. Therefore, [`breadx`] does not
//! do it by default.
//!
//! This module provides a way to process special events. The [`SpecialEventDisplay`]
//! type is a wrapper around a `Display` that provides queues for special events.
//! Queues can be registered or deregistered, and then polled or waited on, similar
//! to regular events.
//!
//! [`SpecialEventDisplay`] can be created through its universal `From` implementation.

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, collections::VecDeque, vec::Vec};
use breadx::{
    display::{Display, DisplayBase, RawReply, RawRequest},
    protocol::Event,
    Result,
};
use slab::Slab;

#[cfg(feature = "async")]
use breadx::display::{AsyncDisplay, AsyncDisplayExt, AsyncStatus, CanBeAsyncDisplay};
#[cfg(feature = "async")]
use core::task::Context;

/// A special type of [`Display`] that provides queues for special events.
///
/// See the [module level documentation](index.html) for more information.
///
/// [`Display`]: breadx::display::Display
pub struct SpecialEventDisplay<Dpy: ?Sized> {
    /// Queue for normal events.
    event_queue: VecDeque<Event>,
    /// Queues for special events.
    special_event_queues: Slab<VecDeque<Event>>,
    /// List of classifiers used to classify the special events.
    classifiers: Vec<Classifier>,
    /// The wrapped display.
    display: Dpy,
}

struct Classifier {
    /// The function used to classify the event.
    classify: Box<dyn FnMut(&Event) -> bool + Send + Sync + 'static>,
    /// The queue to insert the event into.
    key: usize,
}

impl<Dpy> From<Dpy> for SpecialEventDisplay<Dpy> {
    fn from(display: Dpy) -> Self {
        Self {
            event_queue: VecDeque::new(),
            special_event_queues: Slab::new(),
            classifiers: Vec::new(),
            display,
        }
    }
}

impl<Dpy: ?Sized> AsRef<Dpy> for SpecialEventDisplay<Dpy> {
    fn as_ref(&self) -> &Dpy {
        &self.display
    }
}

impl<Dpy: ?Sized> AsMut<Dpy> for SpecialEventDisplay<Dpy> {
    fn as_mut(&mut self) -> &mut Dpy {
        &mut self.display
    }
}

impl<Dpy> SpecialEventDisplay<Dpy> {
    /// Get the inner display type.
    pub fn into_inner(self) -> Dpy {
        self.display
    }
}

impl<Dpy: ?Sized> SpecialEventDisplay<Dpy> {
    /// Create a new special event queue with the given classifier.
    ///
    /// Returns the key to be used to poll the event queue.
    pub fn create_special_event_queue(
        &mut self,
        classifier: impl FnMut(&Event) -> bool + Send + Sync + 'static,
    ) -> usize {
        // create a new queue in the slab
        let key = self.special_event_queues.insert(VecDeque::new());

        // add the classifier to the list of classifiers
        self.classifiers.push(Classifier {
            classify: Box::new(classifier),
            key,
        });

        key
    }

    /// Remove the special event queue with the given key.
    pub fn remove_special_event_queue(&mut self, key: usize) {
        // remove the classifier from the list of classifiers
        self.classifiers.retain(|classifier| classifier.key != key);

        // remove the queue from the slab
        self.special_event_queues.remove(key);
    }

    /// Put a given event into the queue it belongs in.
    fn enqueue_event(&mut self, event: Event) {
        // check if the event is special
        for classifier in &mut self.classifiers {
            if (classifier.classify)(&event) {
                // put the event into the queue
                self.special_event_queues[classifier.key].push_back(event);
                return;
            }
        }

        // if it's not special, put it into the normal queue
        self.event_queue.push_back(event);
    }
}

impl<Dpy: DisplayBase + ?Sized> SpecialEventDisplay<Dpy> {
    /// Poll for an event from the inner display until we find one that we want.
    fn poll_for_some_event(
        &mut self,
        mut until: impl FnMut(&mut Self) -> Option<Event>,
    ) -> Result<Option<Event>> {
        loop {
            if let Some(event) = until(self) {
                return Ok(Some(event));
            }

            // poll for event from the inner display
            let event = match self.display.poll_for_event()? {
                Some(event) => event,
                None => return Ok(None),
            };

            self.enqueue_event(event);
        }
    }

    /// Poll for a special event for the given queue.
    pub fn poll_for_special_event(&mut self, key: usize) -> Result<Option<Event>> {
        self.poll_for_some_event(|this| this.special_event_queues[key].pop_front())
    }
}

impl<Dpy: Display + ?Sized> SpecialEventDisplay<Dpy> {
    /// Wait for an event from the inner display.
    fn wait_for_some_event(
        &mut self,
        mut until: impl FnMut(&mut Self) -> Option<Event>,
    ) -> Result<Event> {
        loop {
            if let Some(event) = until(self) {
                return Ok(event);
            }

            // poll for event from the inner display
            let event = self.display.wait_for_event()?;

            self.enqueue_event(event);
        }
    }

    /// Wait for a special event from the given queue.
    pub fn wait_for_special_event(&mut self, key: usize) -> Result<Event> {
        self.wait_for_some_event(|this| this.special_event_queues[key].pop_front())
    }
}

#[cfg(feature = "async")]
impl<Dpy: AsyncDisplay + ?Sized> SpecialEventDisplay<Dpy> {
    /// Wait for a speical event from the given queue.
    pub async fn wait_for_special_event_async(&mut self, key: usize) -> Result<Event> {
        loop {
            if let Some(event) = self.special_event_queues[key].pop_front() {
                return Ok(event);
            }

            let event = self.display.wait_for_event().await?;

            self.enqueue_event(event);
        }
    }
}

impl<D: DisplayBase + ?Sized> DisplayBase for SpecialEventDisplay<D> {
    fn setup(&self) -> &breadx::protocol::xproto::Setup {
        self.display.setup()
    }

    fn default_screen_index(&self) -> usize {
        self.display.default_screen_index()
    }

    fn poll_for_event(&mut self) -> Result<Option<Event>> {
        self.poll_for_some_event(|this| this.event_queue.pop_front())
    }

    fn poll_for_reply_raw(&mut self, seq: u64) -> Result<Option<RawReply>> {
        self.display.poll_for_reply_raw(seq)
    }
}

impl<D: Display + ?Sized> Display for SpecialEventDisplay<D> {
    fn send_request_raw(&mut self, req: RawRequest<'_, '_>) -> Result<u64> {
        self.display.send_request_raw(req)
    }

    fn flush(&mut self) -> Result<()> {
        self.display.flush()
    }

    fn generate_xid(&mut self) -> Result<u32> {
        self.display.generate_xid()
    }

    fn maximum_request_length(&mut self) -> Result<usize> {
        self.display.maximum_request_length()
    }

    fn synchronize(&mut self) -> Result<()> {
        self.display.synchronize()
    }

    fn wait_for_reply_raw(&mut self, seq: u64) -> Result<RawReply> {
        self.display.wait_for_reply_raw(seq)
    }

    fn wait_for_event(&mut self) -> Result<Event> {
        self.wait_for_some_event(|this| this.event_queue.pop_front())
    }
}

#[cfg(feature = "async")]
impl<D: CanBeAsyncDisplay + ?Sized> CanBeAsyncDisplay for SpecialEventDisplay<D> {
    fn format_request(
        &mut self,
        req: &mut RawRequest<'_, '_>,
        ctx: &mut Context<'_>,
    ) -> Result<AsyncStatus<u64>> {
        self.display.format_request(req, ctx)
    }

    fn try_flush(&mut self, ctx: &mut Context<'_>) -> Result<AsyncStatus<()>> {
        self.display.try_flush(ctx)
    }

    fn try_generate_xid(&mut self, ctx: &mut Context<'_>) -> Result<AsyncStatus<u32>> {
        self.display.try_generate_xid(ctx)
    }

    fn try_maximum_request_length(&mut self, ctx: &mut Context<'_>) -> Result<AsyncStatus<usize>> {
        self.display.try_maximum_request_length(ctx)
    }

    fn try_send_request_raw(
        &mut self,
        req: &mut RawRequest<'_, '_>,
        ctx: &mut Context<'_>,
    ) -> Result<AsyncStatus<()>> {
        self.display.try_send_request_raw(req, ctx)
    }

    fn try_wait_for_event(&mut self, ctx: &mut Context<'_>) -> Result<AsyncStatus<Event>> {
        loop {
            let event = match self.display.try_wait_for_event(ctx)? {
                AsyncStatus::Ready(event) => event,
                stats => return Ok(stats),
            };

            self.enqueue_event(event);

            if let Some(event) = self.event_queue.pop_front() {
                return Ok(AsyncStatus::Ready(event));
            }
        }
    }

    fn try_wait_for_reply_raw(
        &mut self,
        seq: u64,
        ctx: &mut Context<'_>,
    ) -> Result<AsyncStatus<RawReply>> {
        self.display.try_wait_for_reply_raw(seq, ctx)
    }
}

#[cfg(feature = "async")]
impl<D: AsyncDisplay + ?Sized> AsyncDisplay for SpecialEventDisplay<D> {
    fn poll_for_interest(
        &mut self,
        interest: breadx::display::Interest,
        callback: &mut dyn FnMut(&mut dyn AsyncDisplay, &mut Context<'_>) -> Result<()>,
        ctx: &mut Context<'_>,
    ) -> core::task::Poll<Result<()>> {
        self.display.poll_for_interest(interest, callback, ctx)
    }
}
