//! A contract laid out the way sekvent-proto-build generates it: one module
//! per proto package, the service constant next to its messages, a reply
//! from another package, a well-known type and a message without a package.

use std::time::Duration;

use sekvent_component::{App, AppError, CallContext, ComponentHandle, component};
use sekvent_config::MapSource;

pub mod proto {
    pub mod common {
        pub mod v1 {
            #[derive(Clone, PartialEq, prost::Message)]
            pub struct Money {
                #[prost(string, tag = "1")]
                pub currency: String,
                #[prost(int64, tag = "2")]
                pub minor_units: i64,
            }

            impl prost::Name for Money {
                const NAME: &'static str = "Money";
                const PACKAGE: &'static str = "common.v1";
            }
        }
    }

    pub mod check {
        pub mod billing {
            pub mod v1 {
                #[derive(Clone, PartialEq, prost::Message)]
                pub struct TotalRequest {
                    #[prost(string, tag = "1")]
                    pub order_id: String,
                }

                impl prost::Name for TotalRequest {
                    const NAME: &'static str = "TotalRequest";
                    const PACKAGE: &'static str = "check.billing.v1";
                }

                /// Contract of `check.billing.v1.Billing`, checked by `#[component(proto = …)]`.
                #[doc(hidden)]
                #[allow(non_upper_case_globals, dead_code)]
                pub const __sekvent_service_Billing: (&str, &[(&str, &str, &str, bool)]) = (
                    "check.billing.v1.Billing",
                    &[
                        (
                            "Total",
                            "check.billing.v1.TotalRequest",
                            "common.v1.Money",
                            false,
                        ),
                        (
                            "Describe",
                            "google.protobuf.StringValue",
                            "google.protobuf.StringValue",
                            false,
                        ),
                        ("Tag", "Loose", "check.billing.v1.TotalRequest", false),
                    ],
                );
            }
        }
    }
}

/// A message declared in a proto file without a package.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Loose {
    #[prost(string, tag = "1")]
    pub label: String,
}

impl prost::Name for Loose {
    const NAME: &'static str = "Loose";
    const PACKAGE: &'static str = "";
}

use proto::check::billing::v1::TotalRequest;
use proto::common::v1::Money;

/// Prices orders.
#[component(
    name = "billing",
    package = "check.billing.v1",
    proto = "crate::proto::check::billing::v1"
)]
pub trait Billing {
    /// The total of an order.
    #[call(idempotent)]
    async fn total(&self, cx: &CallContext, req: TotalRequest) -> Result<Money, AppError>;

    /// Describe a text.
    #[call]
    async fn describe(&self, cx: &CallContext, req: String) -> Result<String, AppError>;

    /// Turn a label into an order reference.
    #[call]
    async fn tag(&self, cx: &CallContext, req: Loose) -> Result<TotalRequest, AppError>;
}

struct BillingService;

impl Billing for BillingService {
    async fn total(&self, _cx: &CallContext, req: TotalRequest) -> Result<Money, AppError> {
        Ok(Money {
            currency: "EUR".to_owned(),
            minor_units: i64::try_from(req.order_id.len()).unwrap_or_default(),
        })
    }

    async fn describe(&self, _cx: &CallContext, req: String) -> Result<String, AppError> {
        Ok(format!("text of {} bytes", req.len()))
    }

    async fn tag(&self, _cx: &CallContext, req: Loose) -> Result<TotalRequest, AppError> {
        Ok(TotalRequest {
            order_id: req.label,
        })
    }
}

#[tokio::main]
async fn main() {
    let descriptor = <BillingHandle as ComponentHandle>::DESCRIPTOR;
    assert_eq!(
        descriptor.full_service_name().as_deref(),
        Some("check.billing.v1.Billing")
    );

    let source = MapSource::new().with("SEKVENT_COMPONENT_BINDING", "local-serialized");
    let mut builder = App::builder(&source);
    BillingHandle::install(&mut builder, |_deps| Ok(BillingService)).expect("install");
    let app = builder.build().expect("build");
    app.start().await.expect("start");

    let billing = app.handle::<BillingHandle>().expect("handle");
    let cx = CallContext::new();
    let total = billing
        .total(
            &cx,
            TotalRequest {
                order_id: "o-1234".to_owned(),
            },
        )
        .await
        .expect("total");
    assert_eq!((total.currency.as_str(), total.minor_units), ("EUR", 6));
    let text = billing
        .describe(&cx, "hello".to_owned())
        .await
        .expect("describe");
    assert_eq!(text, "text of 5 bytes");
    let tagged = billing
        .tag(
            &cx,
            Loose {
                label: "x".to_owned(),
            },
        )
        .await
        .expect("tag");
    assert_eq!(tagged.order_id, "x");

    app.stop(Duration::from_secs(1)).await.expect("stop");
}
