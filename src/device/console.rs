//! Driver for VirtIO console devices.

use crate::hal::Hal;
use crate::queue::VirtQueue;
use crate::transport::Transport;
use crate::volatile::{volread, ReadOnly, WriteOnly};
use crate::{Result, PAGE_SIZE};
use alloc::boxed::Box;
use bitflags::bitflags;
use core::ptr::NonNull;

const QUEUE_RECEIVEQ_PORT_0: u16 = 0;
const QUEUE_TRANSMITQ_PORT_0: u16 = 1;
const QUEUE_SIZE: usize = 2;
const SUPPORTED_FEATURES: Features = Features::RING_EVENT_IDX.union(Features::RING_INDIRECT_DESC);

/// Driver for a VirtIO console device.
///
/// Only a single port is allowed since `alloc` is disabled. Emergency write and cols/rows are not
/// implemented.
///
/// # Example
///
/// ```
/// # use virtio_drivers::{Error, Hal, transport::Transport};
/// use virtio_drivers::device::console::VirtIOConsole;
/// # fn example<HalImpl: Hal, T: Transport>(transport: T) -> Result<(), Error> {
/// let mut console = VirtIOConsole::<HalImpl, _>::new(transport)?;
///
/// let info = console.info();
/// println!("VirtIO console {}x{}", info.rows, info.columns);
///
/// for &c in b"Hello console!\n" {
///   console.send(c)?;
/// }
///
/// let c = console.recv(true)?;
/// println!("Read {:?} from console.", c);
/// # Ok(())
/// # }
/// ```
pub struct VirtIOConsole<H: Hal, T: Transport> {
    transport: T,
    config_space: NonNull<Config>,
    receiveq: VirtQueue<H, QUEUE_SIZE>,
    transmitq: VirtQueue<H, QUEUE_SIZE>,
    queue_buf_rx: Box<[u8; PAGE_SIZE]>,
    cursor: usize,
    pending_len: usize,
    /// The token of the outstanding receive request, if there is one.
    receive_token: Option<u16>,
}

// SAFETY: The config space can be accessed from any thread.
unsafe impl<H: Hal, T: Transport + Send> Send for VirtIOConsole<H, T> where
    VirtQueue<H, QUEUE_SIZE>: Send
{
}

// SAFETY: A `&VirtIOConsole` only allows reading the config space.
unsafe impl<H: Hal, T: Transport + Sync> Sync for VirtIOConsole<H, T> where
    VirtQueue<H, QUEUE_SIZE>: Sync
{
}

/// Information about a console device, read from its configuration space.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsoleInfo {
    /// The console height in characters.
    pub rows: u16,
    /// The console width in characters.
    pub columns: u16,
    /// The maxumum number of ports supported by the console device.
    pub max_ports: u32,
}

impl<H: Hal, T: Transport> VirtIOConsole<H, T> {
    /// Creates a new VirtIO console driver.
    pub fn new(mut transport: T, hal:H) -> Result<Self> {
        let negotiated_features = transport.begin_init(SUPPORTED_FEATURES);
        let config_space = transport.config_space::<Config>()?;
        let receiveq = VirtQueue::new(
            hal,
            &mut transport,
            QUEUE_RECEIVEQ_PORT_0,
            negotiated_features.contains(Features::RING_INDIRECT_DESC),
            negotiated_features.contains(Features::RING_EVENT_IDX),
        )?;
        let transmitq = VirtQueue::new(
            hal,
            &mut transport,
            QUEUE_TRANSMITQ_PORT_0,
            negotiated_features.contains(Features::RING_INDIRECT_DESC),
            negotiated_features.contains(Features::RING_EVENT_IDX),
        )?;

        // Safe because no alignment or initialisation is required for [u8], the DMA buffer is
        // dereferenceable, and the lifetime of the reference matches the lifetime of the DMA buffer
        // (which we don't otherwise access).
        let queue_buf_rx = Box::new([0; PAGE_SIZE]);

        transport.finish_init();
        let mut console = VirtIOConsole {
            transport,
            config_space,
            receiveq,
            transmitq,
            queue_buf_rx,
            cursor: 0,
            pending_len: 0,
            receive_token: None,
        };
        console.poll_retrieve()?;
        Ok(console)
    }

    /// Returns a struct with information about the console device, such as the number of rows and columns.
    pub fn info(&self) -> ConsoleInfo {
        // Safe because config_space is a valid pointer to the device configuration space.
        unsafe {
            let columns = volread!(self.config_space, cols);
            let rows = volread!(self.config_space, rows);
            let max_ports = volread!(self.config_space, max_nr_ports);
            ConsoleInfo {
                rows,
                columns,
                max_ports,
            }
        }
    }

    /// Makes a request to the device to receive data, if there is not already an outstanding
    /// receive request or some data already received and not yet returned.
    fn poll_retrieve(&mut self) -> Result<()> {
        if self.receive_token.is_none() && self.cursor == self.pending_len {
            // Safe because the buffer lasts at least as long as the queue, and there are no other
            // outstanding requests using the buffer.
            self.receive_token = Some(unsafe {
                self.receiveq
                    .add(&[], &mut [self.queue_buf_rx.as_mut_slice()])
            }?);
            if self.receiveq.should_notify() {
                self.transport.notify(QUEUE_RECEIVEQ_PORT_0);
            }
        }
        Ok(())
    }

    /// Acknowledges a pending interrupt, if any, and completes the outstanding finished read
    /// request if there is one.
    ///
    /// Returns true if new data has been received.
    pub fn ack_interrupt(&mut self) -> Result<bool> {
        if !self.transport.ack_interrupt() {
            return Ok(false);
        }

        self.finish_receive()
    }

    /// If there is an outstanding receive request and it has finished, completes it.
    ///
    /// Returns true if new data has been received.
    fn finish_receive(&mut self) -> Result<bool> {
        let mut flag = false;
        if let Some(receive_token) = self.receive_token {
            if self.receive_token == self.receiveq.peek_used() {
                // Safe because we are passing the same buffer as we passed to `VirtQueue::add` in
                // `poll_retrieve` and it is still valid.
                let len = unsafe {
                    self.receiveq.pop_used(
                        receive_token,
                        &[],
                        &mut [self.queue_buf_rx.as_mut_slice()],
                    )?
                };
                flag = true;
                assert_ne!(len, 0);
                self.cursor = 0;
                self.pending_len = len as usize;
                // Clear `receive_token` so that when the buffer is used up the next call to
                // `poll_retrieve` will add a new pending request.
                self.receive_token.take();
            }
        }
        Ok(flag)
    }

    /// Returns the next available character from the console, if any.
    ///
    /// If no data has been received this will not block but immediately return `Ok<None>`.
    pub fn recv(&mut self, pop: bool) -> Result<Option<u8>> {
        self.finish_receive()?;
        if self.cursor == self.pending_len {
            return Ok(None);
        }
        let ch = self.queue_buf_rx[self.cursor];
        if pop {
            self.cursor += 1;
            self.poll_retrieve()?;
        }
        Ok(Some(ch))
    }

    /// Sends a character to the console.
    pub fn send(&mut self, chr: u8) -> Result<()> {
        let buf: [u8; 1] = [chr];
        self.transmitq
            .add_notify_wait_pop(&[&buf], &mut [], &mut self.transport)?;
        Ok(())
    }
}

impl<H: Hal, T: Transport> Drop for VirtIOConsole<H, T> {
    fn drop(&mut self) {
        // Clear any pointers pointing to DMA regions, so the device doesn't try to access them
        // after they have been freed.
        self.transport.queue_unset(QUEUE_RECEIVEQ_PORT_0);
        self.transport.queue_unset(QUEUE_TRANSMITQ_PORT_0);
    }
}

#[repr(C)]
struct Config {
    cols: ReadOnly<u16>,
    rows: ReadOnly<u16>,
    max_nr_ports: ReadOnly<u32>,
    emerg_wr: WriteOnly<u32>,
}

bitflags! {
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    struct Features: u64 {
        const SIZE                  = 1 << 0;
        const MULTIPORT             = 1 << 1;
        const EMERG_WRITE           = 1 << 2;

        // device independent
        const NOTIFY_ON_EMPTY       = 1 << 24; // legacy
        const ANY_LAYOUT            = 1 << 27; // legacy
        const RING_INDIRECT_DESC    = 1 << 28;
        const RING_EVENT_IDX        = 1 << 29;
        const UNUSED                = 1 << 30; // legacy
        const VERSION_1             = 1 << 32; // detect legacy

        // since virtio v1.1
        const ACCESS_PLATFORM       = 1 << 33;
        const RING_PACKED           = 1 << 34;
        const IN_ORDER              = 1 << 35;
        const ORDER_PLATFORM        = 1 << 36;
        const SR_IOV                = 1 << 37;
        const NOTIFICATION_DATA     = 1 << 38;
    }
}
