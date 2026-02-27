//! Serial console device (8250 UART emulation)
//!
//! Provides a simple serial console using vm-superio's Serial device.
//! The serial port handles I/O at ports 0x3f8-0x3ff (COM1).

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tracing::trace;

/// Serial device state
#[derive(Clone)]
pub struct SerialDevice {
    inner: Arc<Mutex<SerialInner>>,
}

struct SerialInner {
    /// Output channel for serial data
    output_tx: mpsc::Sender<u8>,
    /// Input buffer (for guest reading)
    input_buffer: Vec<u8>,
    /// Line Status Register
    lsr: u8,
    /// Interrupt Enable Register
    ier: u8,
    /// Interrupt Identification Register
    iir: u8,
    /// FIFO Control Register
    fcr: u8,
    /// Line Control Register
    lcr: u8,
    /// Modem Control Register
    mcr: u8,
    /// Divisor Latch (low byte)
    dll: u8,
    /// Divisor Latch (high byte)
    dlh: u8,
    /// Scratch Register
    scr: u8,
}

/// Line Status Register bits
mod lsr {
    /// Data Ready (input available)
    pub const DR: u8 = 1 << 0;
    /// Transmitter Holding Register Empty
    pub const THRE: u8 = 1 << 5;
    /// Transmitter Empty
    pub const TEMT: u8 = 1 << 6;
}

/// Line Control Register bits
mod lcr {
    /// Divisor Latch Access Bit
    pub const DLAB: u8 = 1 << 7;
}

impl SerialDevice {
    /// Create a new serial device with the given output channel
    pub fn new(output_tx: mpsc::Sender<u8>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SerialInner {
                output_tx,
                input_buffer: Vec::new(),
                lsr: lsr::THRE | lsr::TEMT, // Transmitter ready
                ier: 0,
                iir: 0x01, // No interrupt pending
                fcr: 0,
                lcr: 0,
                mcr: 0,
                dll: 0,
                dlh: 0,
                scr: 0,
            })),
        }
    }

    /// Write to a serial port register
    pub fn write(&mut self, offset: u8, value: u8) {
        let mut inner = self.inner.lock().unwrap();

        // Check if DLAB is set for divisor latch access
        let dlab = (inner.lcr & lcr::DLAB) != 0;

        match offset {
            0 => {
                if dlab {
                    // Divisor Latch Low
                    inner.dll = value;
                } else {
                    // Transmit Holding Register - output character
                    trace!("Serial TX: {:02x} '{}'", value, value as char);

                    // Send to output channel
                    let _ = inner.output_tx.try_send(value);
                }
            }
            1 => {
                if dlab {
                    // Divisor Latch High
                    inner.dlh = value;
                } else {
                    // Interrupt Enable Register
                    inner.ier = value;
                }
            }
            2 => {
                // FIFO Control Register
                inner.fcr = value;
            }
            3 => {
                // Line Control Register
                inner.lcr = value;
            }
            4 => {
                // Modem Control Register
                inner.mcr = value;
            }
            7 => {
                // Scratch Register
                inner.scr = value;
            }
            _ => {
                trace!("Serial write to unknown offset {}: {:#x}", offset, value);
            }
        }
    }

    /// Read from a serial port register
    pub fn read(&self, offset: u8) -> u8 {
        let mut inner = self.inner.lock().unwrap();

        // Check if DLAB is set for divisor latch access
        let dlab = (inner.lcr & lcr::DLAB) != 0;

        match offset {
            0 => {
                if dlab {
                    // Divisor Latch Low
                    inner.dll
                } else {
                    // Receive Buffer Register
                    if let Some(byte) = inner.input_buffer.pop() {
                        if inner.input_buffer.is_empty() {
                            inner.lsr &= !lsr::DR;
                        }
                        byte
                    } else {
                        0
                    }
                }
            }
            1 => {
                if dlab {
                    // Divisor Latch High
                    inner.dlh
                } else {
                    // Interrupt Enable Register
                    inner.ier
                }
            }
            2 => {
                // Interrupt Identification Register
                inner.iir
            }
            3 => {
                // Line Control Register
                inner.lcr
            }
            4 => {
                // Modem Control Register
                inner.mcr
            }
            5 => {
                // Line Status Register
                inner.lsr
            }
            6 => {
                // Modem Status Register
                0xb0 // CTS, DSR, DCD active
            }
            7 => {
                // Scratch Register
                inner.scr
            }
            _ => {
                trace!("Serial read from unknown offset {}", offset);
                0xFF
            }
        }
    }

    /// Queue input data for the guest to read
    pub fn queue_input(&self, data: &[u8]) {
        let mut inner = self.inner.lock().unwrap();
        inner.input_buffer.extend(data.iter().rev()); // Reverse for FIFO behavior
        if !inner.input_buffer.is_empty() {
            inner.lsr |= lsr::DR;
        }
    }

    /// Check if there's input available
    pub fn has_input(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        !inner.input_buffer.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_serial_write() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut serial = SerialDevice::new(tx);

        // Write a character
        serial.write(0, b'A');

        // Should receive it
        let byte = rx.recv().await.unwrap();
        assert_eq!(byte, b'A');
    }

    #[test]
    fn test_serial_lsr() {
        let (tx, _rx) = mpsc::channel(16);
        let serial = SerialDevice::new(tx);

        // LSR should show transmitter ready
        let lsr = serial.read(5);
        assert!(lsr & lsr::THRE != 0);
        assert!(lsr & lsr::TEMT != 0);
    }

    #[test]
    fn test_serial_input() {
        let (tx, _rx) = mpsc::channel(16);
        let serial = SerialDevice::new(tx);

        // No input initially
        assert!(!serial.has_input());

        // Queue some input
        serial.queue_input(b"hello");
        assert!(serial.has_input());

        // Read it back (FIFO order)
        assert_eq!(serial.read(0), b'h');
        assert_eq!(serial.read(0), b'e');
    }

    #[test]
    fn test_serial_scratch_register() {
        let (tx, _rx) = mpsc::channel(16);
        let mut serial = SerialDevice::new(tx);

        // Write and read scratch register
        serial.write(7, 0x42);
        assert_eq!(serial.read(7), 0x42);
    }
}
