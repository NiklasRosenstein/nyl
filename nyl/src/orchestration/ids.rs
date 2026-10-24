//! UUIDv7 identifiers for incarnations and runs.

use nyl_core::orchestration::ids::{RunId, Uid};
use nyl_core::orchestration::ports::IdGenerator;

/// Generates UUIDv7s from the system time and random bits, so identifiers
/// sort by creation time.
#[derive(Debug, Clone, Copy, Default)]
pub struct UuidV7;

impl UuidV7 {
    fn generate() -> String {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX));
        let mut random = [0u8; 10];
        getrandom::fill(&mut random).expect("the operating system provides random bytes");
        uuid_v7(millis, random)
    }
}

/// Lay out a UUIDv7: 48 bits of Unix milliseconds, version 7, 12 random
/// bits, the RFC 9562 variant, and 62 random bits.
fn uuid_v7(millis: u64, random: [u8; 10]) -> String {
    let mut bytes = [0u8; 16];
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6..].copy_from_slice(&random);
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    let hex = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

impl IdGenerator for UuidV7 {
    fn uid(&self) -> Uid {
        Uid::new(Self::generate()).expect("UUIDv7 form")
    }

    fn run_id(&self) -> RunId {
        RunId::new(Self::generate()).expect("UUIDv7 form")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uuid_v7_layout_sorts_by_time_and_marks_version_and_variant() {
        let earlier = uuid_v7(1_000, [0xff; 10]);
        let later = uuid_v7(1_001, [0x00; 10]);
        assert!(earlier < later);
        assert_eq!(&earlier[14..15], "7");
        assert!(matches!(&earlier[19..20], "8" | "9" | "a" | "b"));
        assert!(Uid::new(UuidV7.uid().as_str()).is_ok());
    }
}
