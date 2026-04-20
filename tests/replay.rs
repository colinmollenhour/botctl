use std::path::Path;

use botctl::classifier::Classifier;
use botctl::fixtures::{FixtureCase, discover_cases};

#[test]
fn fixture_cases_classify_as_expected() {
    let root = Path::new("fixtures/cases");
    let cases = discover_cases(root).expect("fixture cases should be discoverable");
    assert!(!cases.is_empty(), "expected at least one fixture case");

    let classifier = Classifier;

    for path in cases {
        let case = FixtureCase::load(&path).expect("fixture case should load");
        let classification = classifier.classify(&case.name, &case.frame_text);
        assert_eq!(
            classification.state, case.expected_state,
            "fixture {} did not classify as expected",
            case.name
        );
    }
}
