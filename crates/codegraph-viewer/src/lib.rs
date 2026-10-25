//! Browser code is Rust; wasm-bindgen's generated JavaScript only loads WASM.
#[cfg(target_arch = "wasm32")]
mod browser;
pub fn encode(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(b) {
                (*b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
#[cfg(test)]
mod tests {
    #[test]
    fn url_values_are_encoded() {
        assert_eq!(
            super::encode("function:abc#1 /雪"),
            "function%3Aabc%231%20%2F%E9%9B%AA"
        );
    }
}
