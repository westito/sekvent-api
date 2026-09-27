use prost_build::{Service, ServiceGenerator};

/// Runs several service generators in order, each appending to the same
/// buffer. This is how tonic's stubs and any extra hook (for example future
/// component codegen) share one prost pass.
pub(crate) struct Chain {
    generators: Vec<Box<dyn ServiceGenerator>>,
}

impl Chain {
    pub(crate) fn new(generators: Vec<Box<dyn ServiceGenerator>>) -> Self {
        Self { generators }
    }
}

impl ServiceGenerator for Chain {
    fn generate(&mut self, service: Service, buf: &mut String) {
        for generator in &mut self.generators {
            generator.generate(service.clone(), buf);
        }
    }

    fn finalize(&mut self, buf: &mut String) {
        for generator in &mut self.generators {
            generator.finalize(buf);
        }
    }

    fn finalize_package(&mut self, package: &str, buf: &mut String) {
        for generator in &mut self.generators {
            generator.finalize_package(package, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    struct Tag(&'static str);

    impl ServiceGenerator for Tag {
        fn generate(&mut self, service: Service, buf: &mut String) {
            let _ = write!(buf, "[{} {}]", self.0, service.proto_name);
        }
        fn finalize(&mut self, buf: &mut String) {
            let _ = write!(buf, "<end {}>", self.0);
        }
        fn finalize_package(&mut self, package: &str, buf: &mut String) {
            let _ = write!(buf, "<{package} {}>", self.0);
        }
    }

    fn service() -> Service {
        Service {
            name: "Invoices".to_owned(),
            proto_name: "Invoices".to_owned(),
            package: "billing.v1".to_owned(),
            comments: prost_build::Comments::default(),
            methods: Vec::new(),
            options: prost_types::ServiceOptions::default(),
        }
    }

    #[test]
    fn every_generator_runs_in_order() {
        let mut chain = Chain::new(vec![Box::new(Tag("a")), Box::new(Tag("b"))]);
        let mut buf = String::new();
        chain.generate(service(), &mut buf);
        chain.finalize_package("billing.v1", &mut buf);
        chain.finalize(&mut buf);
        assert_eq!(
            buf,
            "[a Invoices][b Invoices]<billing.v1 a><billing.v1 b><end a><end b>"
        );
    }
}
