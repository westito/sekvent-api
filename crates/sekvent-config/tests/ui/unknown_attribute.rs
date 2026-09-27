use sekvent_config::EnvConfig;

#[derive(EnvConfig)]
struct Config {
    #[config(colour = "red")]
    port: u16,
}

fn main() {}
