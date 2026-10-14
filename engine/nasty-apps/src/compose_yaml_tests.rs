use super::*;

#[test]
fn compose_yaml_expands_anchors_and_explicit_overrides() {
    let yaml = r#"
x-defaults: &defaults
  image: fixture:1.0
  user: "1000:1001"
  volumes:
    - /fs/pool/data:/data:ro
  environment:
    PUID: 1000
    TOKEN: ${TOKEN:-unchanged}
services:
  app:
    <<: *defaults
    image: fixture:2.0
"#;
    let parsed = parse_compose_yaml(yaml).unwrap();
    let service = &parsed["services"]["app"];
    assert_eq!(service["image"], "fixture:2.0");
    assert_eq!(service["environment"]["PUID"], 1000);
    assert_eq!(service["environment"]["TOKEN"], "${TOKEN:-unchanged}");
    let binds = extract_compose_binds(yaml);
    assert_eq!(binds.len(), 1);
    assert_eq!(binds[0].host_path, "/fs/pool/data");
    assert_eq!(binds[0].expected_uid, Some(1000));
    assert_eq!(binds[0].expected_gid, Some(1001));
}

#[test]
fn compose_yaml_preserves_literal_and_folded_commands_and_scalars() {
    let parsed = parse_compose_yaml(
        r#"
services:
  app:
    privileged: false
    command: |
      echo first
      echo second
    entrypoint: >-
      sh -c
      'echo ok'
    environment:
      QUOTED: "true"
      BOOL: true
      PORT: 8080
      UNSET: null
"#,
    )
    .unwrap();
    let service = &parsed["services"]["app"];
    assert_eq!(service["privileged"], false);
    assert_eq!(service["command"], "echo first\necho second\n");
    assert_eq!(service["entrypoint"], "sh -c 'echo ok'");
    assert_eq!(service["environment"]["QUOTED"], "true");
    assert_eq!(service["environment"]["BOOL"], true);
    assert_eq!(service["environment"]["PORT"], 8080);
    assert!(service["environment"]["UNSET"].is_null());
}

#[test]
fn compose_yaml_rejects_duplicates_and_multiple_documents() {
    for yaml in [
        "services:\n  app:\n    privileged: true\n    privileged: false\n",
        "services:\n  app:\n    privileged: false\n    privileged: true\n",
        "services: {}\n---\nservices:\n  app:\n    privileged: true\n",
    ] {
        assert!(
            parse_compose_yaml(yaml).is_err(),
            "ambiguous YAML accepted: {yaml}"
        );
    }
}

#[test]
fn compose_yaml_rejects_recursive_and_undefined_aliases() {
    for yaml in [
        "services: *missing\n",
        "x-loop: &loop {again: *loop}\nservices: {}\n",
    ] {
        assert!(parse_compose_yaml(yaml).is_err());
    }
}

#[test]
fn compose_yaml_errors_keep_locations_without_source_snippets() {
    let yaml = "services:\n  app:\n    environment:\n      TOKEN: PRIVATE-SENTINEL-NOT-FOR-ERRORS\n    ports: [80, 81\n";
    let error = parse_compose_yaml(yaml).unwrap_err();
    assert!(error.line().is_some_and(|line| line >= 5));
    assert!(
        !error
            .to_string()
            .contains("PRIVATE-SENTINEL-NOT-FOR-ERRORS")
    );
    assert!(!error.to_string().contains("TOKEN:"));
}

#[test]
fn compose_inventory_resolves_merged_image_and_pull_policy() {
    let yaml = "x-base: &base\n  image: fixture:1.0\n  pull_policy: weekly\nservices:\n  app:\n    <<: *base\n";
    let (names, images, policies) = parse_compose_inventory(yaml, None).unwrap();
    assert_eq!(names, vec!["app"]);
    assert_eq!(images, vec!["fixture:1.0"]);
    assert_eq!(
        policies.get("fixture:1.0"),
        Some(&ComposePullPolicy::Refresh(TimeDelta::days(7)))
    );
}
