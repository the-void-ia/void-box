//! Guest memory management utilities

use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};

use crate::{Error, Result};

/// Write data to guest memory at the specified address
pub fn write_to_guest(
    memory: &GuestMemoryMmap,
    addr: GuestAddress,
    data: &[u8],
) -> Result<()> {
    memory
        .write(data, addr)
        .map(|_| ())
        .map_err(|e| Error::Memory(format!("Failed to write to guest memory at {:#x}: {}", addr.raw_value(), e)))
}

/// Read data from guest memory at the specified address
pub fn read_from_guest(
    memory: &GuestMemoryMmap,
    addr: GuestAddress,
    size: usize,
) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; size];
    memory
        .read(&mut buf, addr)
        .map_err(|e| Error::Memory(format!("Failed to read from guest memory at {:#x}: {}", addr.raw_value(), e)))?;
    Ok(buf)
}

/// Write a slice of data to guest memory, returning the end address
pub fn write_slice_to_guest(
    memory: &GuestMemoryMmap,
    start: GuestAddress,
    data: &[u8],
) -> Result<GuestAddress> {
    write_to_guest(memory, start, data)?;
    Ok(GuestAddress(start.raw_value() + data.len() as u64))
}

/// Zero out a region of guest memory
pub fn zero_guest_memory(
    memory: &GuestMemoryMmap,
    addr: GuestAddress,
    size: usize,
) -> Result<()> {
    let zeros = vec![0u8; size];
    write_to_guest(memory, addr, &zeros)
}

/// Check if an address range fits within guest memory
pub fn check_guest_address(
    memory: &GuestMemoryMmap,
    addr: GuestAddress,
    size: usize,
) -> Result<()> {
    let end_addr = addr
        .raw_value()
        .checked_add(size as u64)
        .ok_or_else(|| Error::Memory("Address overflow".into()))?;

    if !memory.address_in_range(GuestAddress(end_addr.saturating_sub(1))) {
        return Err(Error::Memory(format!(
            "Address range {:#x}-{:#x} outside guest memory",
            addr.raw_value(),
            end_addr
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1024 * 1024)]).unwrap()
    }

    #[test]
    fn test_write_read_guest() {
        let memory = create_test_memory();
        let data = b"hello world";
        let addr = GuestAddress(0x1000);

        write_to_guest(&memory, addr, data).unwrap();
        let read_back = read_from_guest(&memory, addr, data.len()).unwrap();

        assert_eq!(&read_back, data);
    }

    #[test]
    fn test_write_slice() {
        let memory = create_test_memory();
        let data = b"test";
        let start = GuestAddress(0x2000);

        let end = write_slice_to_guest(&memory, start, data).unwrap();
        assert_eq!(end.raw_value(), 0x2000 + 4);
    }

    #[test]
    fn test_zero_memory() {
        let memory = create_test_memory();
        let addr = GuestAddress(0x3000);

        // Write some data
        write_to_guest(&memory, addr, &[1, 2, 3, 4]).unwrap();

        // Zero it out
        zero_guest_memory(&memory, addr, 4).unwrap();

        // Verify it's zeroed
        let data = read_from_guest(&memory, addr, 4).unwrap();
        assert_eq!(data, vec![0, 0, 0, 0]);
    }

    #[test]
    fn test_check_address_valid() {
        let memory = create_test_memory();
        assert!(check_guest_address(&memory, GuestAddress(0), 100).is_ok());
    }

    #[test]
    fn test_check_address_invalid() {
        let memory = create_test_memory();
        // Try to access beyond memory
        assert!(check_guest_address(&memory, GuestAddress(1024 * 1024), 100).is_err());
    }
}
