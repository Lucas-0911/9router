use nine_router_core::{Catalog, model};
use serde_json::Value;

#[test]
fn rust_model_resolution_matches_the_upstream_javascript_reference() {
    let fixture: Value =
        serde_json::from_str(include_str!("../upstream-model-cases.json")).unwrap();
    let catalog = Catalog::load().unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let input = case["input"].as_str();
        let parsed = model::parse(&catalog, input);
        assert_eq!(
            serde_json::to_value(parsed).unwrap(),
            case["parsed"],
            "parse: {input:?}"
        );
        let resolved = model::resolve(&catalog, input, fixture["aliases"].as_object().unwrap());
        assert_eq!(
            serde_json::to_value(resolved).unwrap(),
            case["resolved"],
            "resolve: {input:?}"
        );
    }
}
