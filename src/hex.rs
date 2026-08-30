pub(crate) fn encode_lower(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    let bytes = bytes.as_ref();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(DIGITS[usize::from(byte >> 4)] as char);
        encoded.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::encode_lower;

    #[test]
    fn encodes_every_nibble_and_preserves_leading_zeroes() {
        assert_eq!(encode_lower([0x00, 0x0f, 0x10, 0xab, 0xff]), "000f10abff");
    }
}
