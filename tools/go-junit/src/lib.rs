use quick_junit::{NonSuccessKind, Report, TestCase, TestCaseStatus, TestSuite};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct GoEvent {
    #[serde(rename = "Action")]
    action: String,
    #[serde(rename = "Package", default)]
    package: String,
    #[serde(rename = "ImportPath", default)]
    import_path: Option<String>,
    #[serde(rename = "Test", default)]
    test: Option<String>,
    #[serde(rename = "Elapsed", default)]
    elapsed: Option<f64>,
    #[serde(rename = "Output", default)]
    output: Option<String>,
}

#[derive(Default)]
struct CaseData {
    package: String,
    name: String,
    elapsed: Option<f64>,
    output: String,
    action: String,
}

fn event_package(event: &GoEvent) -> String {
    if !event.package.is_empty() {
        return event.package.clone();
    }
    event
        .import_path
        .as_deref()
        .unwrap_or_default()
        .split_once(" [")
        .map_or_else(
            || event.import_path.clone().unwrap_or_default(),
            |(package, _)| package.to_string(),
        )
}

pub fn convert<R: BufRead>(reader: R) -> Result<(Report, bool), serde_json::Error> {
    let mut cases = BTreeMap::<(String, String), CaseData>::new();
    let mut package_failures = BTreeMap::<String, String>::new();
    let mut failed_packages = BTreeSet::new();

    for line in reader.lines() {
        let line = line.map_err(serde_json::Error::io)?;
        if line.trim().is_empty() {
            continue;
        }
        let event: GoEvent = serde_json::from_str(&line)?;
        if event.test.is_none() {
            let package = event_package(&event);
            if matches!(event.action.as_str(), "output" | "fail" | "build-output") {
                package_failures
                    .entry(package.clone())
                    .or_default()
                    .push_str(event.output.as_deref().unwrap_or(""));
            }
            if matches!(event.action.as_str(), "fail" | "build-fail") {
                failed_packages.insert(package);
            }
            continue;
        }
        let Some(test) = event.test else {
            continue;
        };
        let key = (event.package.clone(), test.clone());
        let case = cases.entry(key).or_insert_with(|| CaseData {
            package: event.package,
            name: test,
            ..CaseData::default()
        });
        if let Some(elapsed) = event.elapsed {
            case.elapsed = Some(elapsed);
        }
        if let Some(output) = event.output {
            case.output.push_str(&output);
        }
        if matches!(event.action.as_str(), "pass" | "fail" | "skip") {
            case.action = event.action;
        }
    }

    for (package, output) in package_failures {
        if !failed_packages.contains(&package) {
            continue;
        }
        if !cases
            .keys()
            .any(|(case_package, _)| case_package == &package)
        {
            cases.insert(
                (package.clone(), "package build".to_string()),
                CaseData {
                    package,
                    name: "package build".to_string(),
                    output,
                    action: "fail".to_string(),
                    ..CaseData::default()
                },
            );
        }
    }

    let had_failure = cases.values().any(|case| case.action == "fail");
    let mut suites = BTreeMap::<String, TestSuite>::new();
    for case in cases.into_values() {
        let suite = suites
            .entry(case.package.clone())
            .or_insert_with(|| TestSuite::new(case.package.clone()));
        let mut status = match case.action.as_str() {
            "skip" => TestCaseStatus::skipped(),
            "fail" => TestCaseStatus::non_success(NonSuccessKind::Failure),
            _ => TestCaseStatus::success(),
        };
        if case.action == "fail" {
            status.set_description(case.output.clone());
        }
        let mut test_case = TestCase::new(case.name, status);
        if let Some(elapsed) = case.elapsed {
            test_case.set_time(std::time::Duration::from_secs_f64(elapsed));
        }
        if !case.output.is_empty() {
            test_case.set_system_out(case.output);
        }
        suite.add_test_case(test_case);
    }

    let mut report = Report::new("go test");
    report.add_test_suites(suites.into_values());
    Ok((report, had_failure))
}

pub fn write_report<R: BufRead>(
    reader: R,
    path: impl AsRef<Path>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let (report, had_failure) = convert(reader)?;
    if let Some(parent) = path.as_ref().parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, report.to_string()?)?;
    Ok(had_failure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn converts_pass_failure_and_skip_events() {
        let fixture = r#"{"Action":"run","Package":"example/pkg","Test":"TestPass"}
{"Action":"output","Package":"example/pkg","Test":"TestPass","Output":"pass output\n"}
{"Action":"pass","Package":"example/pkg","Test":"TestPass","Elapsed":0.01}
{"Action":"run","Package":"example/pkg","Test":"TestFail"}
{"Action":"output","Package":"example/pkg","Test":"TestFail","Output":"failure output\n"}
{"Action":"fail","Package":"example/pkg","Test":"TestFail","Elapsed":0.02}
{"Action":"run","Package":"example/pkg","Test":"TestSkip"}
{"Action":"skip","Package":"example/pkg","Test":"TestSkip","Elapsed":0}
"#;
        let (report, failed) = convert(Cursor::new(fixture)).unwrap();
        assert!(failed);
        assert_eq!(report.tests, 3);
        assert_eq!(report.failures, 1);
        assert_eq!(report.skipped, 1);
        let xml = report.to_string().unwrap();
        assert!(xml.contains("TestPass"));
        assert!(xml.contains("failure output"));
    }

    #[test]
    fn package_build_failure_is_not_an_empty_green_report() {
        let fixture = r#"{"Action":"fail","Package":"example/pkg","Output":"compile error\n"}
"#;
        let (report, failed) = convert(Cursor::new(fixture)).unwrap();
        assert!(failed);
        assert_eq!(report.tests, 1);
        assert_eq!(report.failures, 1);
        assert!(report.to_string().unwrap().contains("package build"));
    }

    #[test]
    fn package_build_failure_includes_build_diagnostics() {
        let fixture = r#"{"ImportPath":"example/gt/emptypkg [example/gt/emptypkg.test]","Action":"build-output","Output":"emptypkg/broken_test.go:5:33: undefined: undefinedSymbol\n"}
{"ImportPath":"example/gt/emptypkg [example/gt/emptypkg.test]","Action":"build-fail"}
"#;
        let (report, failed) = convert(Cursor::new(fixture)).unwrap();
        assert!(failed);
        assert_eq!(report.tests, 1);
        assert_eq!(report.failures, 1);
        let xml = report.to_string().unwrap();
        assert!(xml.contains("undefined: undefinedSymbol"));
    }

    #[test]
    fn package_without_test_files_is_not_a_failure() {
        let fixture = r#"{"Action":"output","Package":"example/gt/emptypkg","Output":"?   \texample/gt/emptypkg\t[no test files]\n"}
{"Action":"skip","Package":"example/gt/emptypkg","Elapsed":0}
"#;
        let (report, failed) = convert(Cursor::new(fixture)).unwrap();
        assert!(!failed);
        assert_eq!(report.tests, 0);
        assert_eq!(report.failures, 0);
        assert!(!report.to_string().unwrap().contains("package build"));
    }
}
