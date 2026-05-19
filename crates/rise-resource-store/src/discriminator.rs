use rand::RngExt;

const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

pub fn generate() -> String {
    let mut rng = rand::rng();
    (0..8)
        .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
        .collect()
}
