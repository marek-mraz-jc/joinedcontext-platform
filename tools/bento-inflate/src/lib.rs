//! The bounded inflate of a zip member for Bento's `wasm` processor (T-3953, PL-72).
//!
//! Bloblang's `decompress("flate")` has no output limit, so a member whose directory lies
//! about its size would inflate whole before the size check. This module stops the inflate at
//! the size the directory declares: it reads the limit from the message's `inflate_limit`
//! metadata, replaces the packed bytes with the inflated ones, and on a refusal leaves the bytes
//! as they were and writes the reason to `inflate_error`, which the next mapping throws. A
//! message without `inflate_limit` (a stored member) passes untouched.
//!
//! The ABI is Bento's: the host calls the exported `inflate` per message, reaches the message
//! through the `bento_wasm` imports, and moves bytes with the exported `allocate` and
//! `deallocate`. Nothing here panics on input, so a pooled module never traps.

use miniz_oxide::inflate::{decompress_slice_iter_to_slice, TINFLStatus};

/// Raw deflate (zip method 8) to at most `limit` bytes, or why not. The output is allocated
/// once at the limit, so memory is the packed bytes plus the declared size, never more.
pub fn inflate_within(packed: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut out = vec![0; limit];
    match decompress_slice_iter_to_slice(&mut out, std::iter::once(packed), false, true) {
        Ok(written) => {
            out.truncate(written);
            Ok(out)
        }
        Err(TINFLStatus::HasMoreOutput) => Err(format!(
            "inflates past the {limit} bytes its directory says"
        )),
        Err(status) => Err(format!("is no deflate stream ({status:?})")),
    }
}

/// The `inflate_limit` metadata as a byte count; empty is no limit asked for.
pub fn limit_of(meta: &[u8]) -> Result<Option<usize>, String> {
    if meta.is_empty() {
        return Ok(None);
    }
    std::str::from_utf8(meta)
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Some)
        .ok_or_else(|| "inflate_limit is no byte count".to_string())
}

#[cfg(target_arch = "wasm32")]
mod abi {
    #[link(wasm_import_module = "bento_wasm")]
    extern "C" {
        fn v0_msg_as_bytes() -> u64;
        fn v0_msg_set_bytes(ptr: *const u8, len: u32);
        fn v0_msg_get_meta(key_ptr: *const u8, key_len: u32) -> u64;
        fn v0_msg_set_meta(key_ptr: *const u8, key_len: u32, ptr: *const u8, len: u32);
    }

    /// Host-written bytes, owned from here on: (pointer << 32) | length.
    unsafe fn take(packed: u64) -> Vec<u8> {
        let (ptr, len) = ((packed >> 32) as usize as *mut u8, packed as u32 as usize);
        if len == 0 {
            return Vec::new();
        }
        Vec::from_raw_parts(ptr, len, len)
    }

    /// Bytes handed to the host, which frees every pointer it reads through `deallocate`: a
    /// static string handed over would be freed into the allocator and corrupt it.
    fn give(bytes: impl Into<Box<[u8]>>) -> (*const u8, u32) {
        let bytes = std::mem::ManuallyDrop::new(bytes.into());
        (bytes.as_ptr(), bytes.len() as u32)
    }

    fn meta(key: &str) -> Vec<u8> {
        let (key, key_len) = give(key.as_bytes());
        unsafe { take(v0_msg_get_meta(key, key_len)) }
    }

    fn set_meta(key: &str, value: &str) {
        let (key, key_len) = give(key.as_bytes());
        let (value, len) = give(value.as_bytes());
        unsafe { v0_msg_set_meta(key, key_len, value, len) }
    }

    /// Memory the host writes a message's bytes into; `take` owns it afterwards.
    #[no_mangle]
    pub extern "C" fn allocate(len: u32) -> *mut u8 {
        let mut buf = std::mem::ManuallyDrop::new(Vec::<u8>::with_capacity(len as usize));
        buf.as_mut_ptr()
    }

    /// The host has copied bytes handed to it by `give`.
    #[no_mangle]
    pub unsafe extern "C" fn deallocate(ptr: *mut u8, len: u32) {
        drop(Vec::from_raw_parts(ptr, len as usize, len as usize));
    }

    #[no_mangle]
    pub extern "C" fn inflate() {
        let limit = match super::limit_of(&meta("inflate_limit")) {
            Ok(Some(limit)) => limit,
            Ok(None) => return,
            Err(reason) => return set_meta("inflate_error", &reason),
        };
        let packed = unsafe { take(v0_msg_as_bytes()) };
        match super::inflate_within(&packed, limit) {
            Ok(body) => {
                drop(packed);
                let (body, len) = give(body);
                unsafe { v0_msg_set_bytes(body, len) }
            }
            Err(reason) => set_meta("inflate_error", &reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use miniz_oxide::deflate::compress_to_vec;

    #[test]
    fn a_member_inflates_to_its_declared_size_and_no_further() {
        let text = b"stop_id,stop_name\n1,Rautatientori\n".repeat(1000);
        let packed = compress_to_vec(&text, 6);
        assert_eq!(inflate_within(&packed, text.len()).unwrap(), text);
        // Fewer bytes than declared come back as they are; the mapping after refuses them.
        assert_eq!(inflate_within(&packed, text.len() + 7).unwrap(), text);
        // A directory that says 100 bytes: refused having produced at most 100.
        let refused = inflate_within(&packed, 100).unwrap_err();
        assert_eq!(refused, "inflates past the 100 bytes its directory says");
    }

    #[test]
    fn a_bomb_is_refused_at_the_limit_not_after_inflating() {
        // 5 MB of zeros packs to about 5 kB; the limit stops it at 100 bytes of output.
        let packed = compress_to_vec(&vec![0u8; 5_000_000], 9);
        assert!(packed.len() < 10_000);
        assert!(inflate_within(&packed, 100).is_err());
    }

    #[test]
    fn garbage_and_an_empty_member_are_named() {
        assert!(inflate_within(b"\xff\xff\xff\xff", 100)
            .unwrap_err()
            .starts_with("is no deflate stream"));
        assert_eq!(inflate_within(&compress_to_vec(b"", 6), 0).unwrap(), b"");
    }

    #[test]
    fn the_limit_is_a_plain_byte_count_or_absent() {
        assert_eq!(limit_of(b"").unwrap(), None);
        assert_eq!(limit_of(b"1126589").unwrap(), Some(1_126_589));
        for bad in [&b"-1"[..], b"1e3", b" 5", b"\xff"] {
            assert!(limit_of(bad).is_err(), "{bad:?}");
        }
    }
}
