use sekvent_config::EnvConfig;

#[derive(EnvConfig)]
struct Config {
    #[config(secret)]
    token: String,
}

fn main() {}
