use sekvent_config::EnvConfig;

#[derive(EnvConfig)]
struct Inner {
    value: u8,
}

#[derive(EnvConfig)]
struct Outer {
    #[config(nested, default = "x")]
    inner: Inner,
}

fn main() {}
