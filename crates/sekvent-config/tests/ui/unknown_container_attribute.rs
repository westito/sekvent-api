use sekvent_config::EnvConfig;

#[derive(EnvConfig)]
#[config(namespace = "BILLING_")]
struct Config {
    port: u16,
}

fn main() {}
