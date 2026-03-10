pub mod discovery;
pub mod ffi;
pub mod gossip;
pub mod message;
pub mod peer;
pub mod persistence;
pub mod relay;
pub mod security;
pub mod topology;
pub mod transport;

pub fn add(left: u64, right: u64) -> u64 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
