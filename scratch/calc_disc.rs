use sha2::{Sha256, Digest};

fn main() {
    let mut hasher = Sha256::new();
    hasher.update(b"account:Registry");
    let result = hasher.finalize();
    println!("Registry Discriminator: {:?}", &result[..8]);

    let mut hasher = Sha256::new();
    hasher.update(b"account:RegistryShard");
    let result = hasher.finalize();
    println!("RegistryShard Discriminator: {:?}", &result[..8]);
}
