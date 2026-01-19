use anyhow::{Result, anyhow};
use serde_yaml::{Mapping, Value};

pub fn build_traefik_file_yaml(lines: Vec<impl Into<String>>) -> Result<String> {
    use serde_yaml::{Mapping, Value};

    let mut root = Value::Mapping(Mapping::new());

    for line in lines {
        let (path, value) = parse_assignment(line.into())?;
        insert(&mut root, &path, value);
    }

    let unwrapped = match root {
        Value::Mapping(mut map) => match map.remove(Value::String("traefik".to_string())) {
            Some(Value::Mapping(inner)) => Value::Mapping(inner),
            Some(other) => other,
            None => Value::Mapping(map),
        },
        other => other,
    };

    Ok(serde_yaml::to_string(&unwrapped)?)
}

#[derive(Debug)]
enum PathItem {
    Key(String),
    KeyIndex(String, usize),
}

fn parse_path(s: &str) -> Vec<PathItem> {
    s.split('.')
        .map(|part| {
            if let Some(open) = part.find('[') {
                if part.ends_with(']') {
                    let name = &part[..open];
                    let idx_str = &part[open + 1..part.len() - 1];
                    let idx = idx_str.parse::<usize>().unwrap_or(0);
                    PathItem::KeyIndex(name.to_string(), idx)
                } else {
                    PathItem::Key(part.to_string())
                }
            } else {
                PathItem::Key(part.to_string())
            }
        })
        .collect()
}

fn parse_assignment(line: String) -> Result<(Vec<PathItem>, Value)> {
    let parts: Vec<&str> = line.splitn(2, '=').collect();
    if parts.len() != 2 {
        return Err(anyhow!("missing '=' in assignment"));
    }
    let key = parts[0].trim();
    let raw_value = parts[1].trim();

    let value = match serde_yaml::from_str::<Value>(raw_value) {
        Ok(v) => v,
        Err(_) => {
            let s = raw_value
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| {
                    raw_value
                        .strip_prefix('\'')
                        .and_then(|s| s.strip_suffix('\''))
                })
                .unwrap_or(raw_value)
                .to_string();
            Value::String(s)
        }
    };

    Ok((parse_path(key), value))
}

fn ensure_mapping(v: &mut Value) -> &mut Mapping {
    if !v.is_mapping() {
        *v = Value::Mapping(Mapping::new());
    }
    v.as_mapping_mut().unwrap()
}

fn ensure_sequence_for_key<'a>(mapping: &'a mut Mapping, key: &'a str) -> &'a mut Vec<Value> {
    let k = Value::String(key.to_string());
    if !mapping.contains_key(&k) {
        mapping.insert(k.clone(), Value::Sequence(Vec::new()));
    }
    mapping
        .get_mut(&k)
        .unwrap()
        .as_sequence_mut()
        .expect("value is not a sequence")
}

fn ensure_mapping_for_key<'a>(mapping: &'a mut Mapping, key: &'a str) -> &'a mut Value {
    let k = Value::String(key.to_string());
    if !mapping.contains_key(&k) {
        mapping.insert(k.clone(), Value::Mapping(Mapping::new()));
    }
    mapping.get_mut(&k).unwrap()
}

fn insert(root: &mut Value, path: &[PathItem], val: Value) {
    if path.is_empty() {
        *root = val;
        return;
    }

    let mut cur = root;
    for (i, item) in path.iter().enumerate() {
        let is_last = i == path.len() - 1;
        match item {
            PathItem::Key(k) => {
                let mapping = ensure_mapping(cur);
                if is_last {
                    mapping.insert(Value::String(k.clone()), val);
                    return;
                } else {
                    cur = ensure_mapping_for_key(mapping, k);
                }
            }
            PathItem::KeyIndex(k, idx) => {
                let mapping = ensure_mapping(cur);
                let seq = ensure_sequence_for_key(mapping, k);
                while seq.len() <= *idx {
                    seq.push(Value::Null);
                }
                if is_last {
                    seq[*idx] = val;
                    return;
                } else {
                    if !seq[*idx].is_mapping() {
                        seq[*idx] = Value::Mapping(Mapping::new());
                    }
                    cur = &mut seq[*idx];
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_yaml::{Mapping, Value};

    fn yaml(input: &[&str]) -> Value {
        let mut root = Value::Mapping(Mapping::new());
        for line in input {
            let (path, value) = parse_assignment(line.to_string()).unwrap();
            insert(&mut root, &path, value);
        }
        root
    }

    #[test]
    fn simple_nested_keys() {
        let v = yaml(&[r#"a.b.c = "value""#]);

        let expected = serde_yaml::from_str::<Value>(
            r#"
a:
  b:
    c: value
"#,
        )
        .unwrap();

        assert_eq!(v, expected);
    }

    #[test]
    fn array_index_creates_sequence() {
        let v = yaml(&[r#"a.items[0].name = "foo""#]);

        let expected = serde_yaml::from_str::<Value>(
            r#"
a:
  items:
    - name: foo
"#,
        )
        .unwrap();

        assert_eq!(v, expected);
    }

    #[test]
    fn sparse_array_is_filled_with_nulls() {
        let v = yaml(&[r#"a.items[2] = "x""#]);

        let expected = serde_yaml::from_str::<Value>(
            r#"
a:
  items:
    - null
    - null
    - x
"#,
        )
        .unwrap();

        assert_eq!(v, expected);
    }

    #[test]
    fn multiple_assignments_merge_tree() {
        let v = yaml(&[r#"a.b.c = 1"#, r#"a.b.d = 2"#, r#"a.e = 3"#]);

        let expected = serde_yaml::from_str::<Value>(
            r#"
a:
  b:
    c: 1
    d: 2
  e: 3
"#,
        )
        .unwrap();

        assert_eq!(v, expected);
    }

    #[test]
    fn overwrite_scalar_with_mapping() {
        let v = yaml(&[r#"a.b = "scalar""#, r#"a.b.c = "nested""#]);

        let expected = serde_yaml::from_str::<Value>(
            r#"
a:
  b:
    c: nested
"#,
        )
        .unwrap();

        assert_eq!(v, expected);
    }

    #[test]
    fn overwrite_mapping_with_scalar() {
        let v = yaml(&[r#"a.b.c = "nested""#, r#"a.b = "scalar""#]);

        let expected = serde_yaml::from_str::<Value>(
            r#"
a:
  b: scalar
"#,
        )
        .unwrap();

        assert_eq!(v, expected);
    }

    #[test]
    fn mixed_key_and_index_depth() {
        let v = yaml(&[r#"x.y[0].z = true"#, r#"x.y[1].z = false"#]);

        let expected = serde_yaml::from_str::<Value>(
            r#"
x:
  y:
    - z: true
    - z: false
"#,
        )
        .unwrap();

        assert_eq!(v, expected);
    }

    #[test]
    fn order_of_assignments_does_not_matter() {
        let v1 = yaml(&[r#"a.b.c = 1"#, r#"a.b.d = 2"#]);

        let v2 = yaml(&[r#"a.b.d = 2"#, r#"a.b.c = 1"#]);

        assert_eq!(v1, v2);
    }

    #[test]
    fn example_from_traefik() {
        let v = yaml(&[
            r#"traefik.http.routers.my_router.tls.domains[0].main = "*.some.com""#,
            r#"traefik.http.routers.my_router.entrypoints = "websecure""#,
        ]);

        let expected = serde_yaml::from_str::<Value>(
            r#"
traefik:
  http:
    routers:
      my_router:
        entrypoints: websecure
        tls:
          domains:
            - main: "*.some.com"
"#,
        )
        .unwrap();

        assert_eq!(v, expected);
    }

    fn normalize_yaml(s: &str) -> Value {
        serde_yaml::from_str::<Value>(s).unwrap()
    }

    #[test]
    fn unwraps_traefik_root_basic() {
        let yaml = build_traefik_file_yaml(vec![
            r#"traefik.http.routers.my_router.entrypoints = "websecure""#,
            r#"traefik.http.routers.my_router.rule = "Host(`example.com`)""#,
        ])
        .unwrap();

        let expected = normalize_yaml(
            r#"
http:
  routers:
    my_router:
      entrypoints: websecure
      rule: Host(`example.com`)
"#,
        );

        assert_eq!(normalize_yaml(&yaml), expected);
    }

    #[test]
    fn preserves_multiple_traefik_children() {
        let yaml = build_traefik_file_yaml(vec![
            r#"traefik.http.routers.r1.rule = "Host(`a.example.com`)""#,
            r#"traefik.http.services.s1.loadbalancer.servers[0].url = "http://1.1.1.1""#,
            r#"traefik.tcp.routers.t1.rule = "HostSNI(`*`)""#,
        ])
        .unwrap();

        let expected = normalize_yaml(
            r#"
http:
  routers:
    r1:
      rule: Host(`a.example.com`)
  services:
    s1:
      loadbalancer:
        servers:
          - url: http://1.1.1.1
tcp:
  routers:
    t1:
      rule: HostSNI(`*`)
"#,
        );

        assert_eq!(normalize_yaml(&yaml), expected);
    }

    #[test]
    fn matches_docs_style_example() {
        let yaml = build_traefik_file_yaml(vec![
            r#"traefik.http.routers.router0.rule = "Host(`foo.bar`)""#,
            r#"traefik.http.routers.router0.service = "service0""#,
            r#"traefik.http.services.service0.loadbalancer.servers[0].url = "http://10.0.0.1""#,
        ])
        .unwrap();

        let expected = normalize_yaml(
            r#"
http:
  routers:
    router0:
      rule: Host(`foo.bar`)
      service: service0
  services:
    service0:
      loadbalancer:
        servers:
          - url: http://10.0.0.1
"#,
        );

        assert_eq!(normalize_yaml(&yaml), expected);
    }

    #[test]
    fn no_traefik_root_is_left_untouched() {
        let yaml = build_traefik_file_yaml(vec![r#"http.routers.r1.rule = "Host(`x`)""#]).unwrap();

        let expected = normalize_yaml(
            r#"
http:
  routers:
    r1:
      rule: Host(`x`)
"#,
        );

        assert_eq!(normalize_yaml(&yaml), expected);
    }
}

#[cfg(all(test, feature = "proptests"))]
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use serde_yaml::Value;

    fn key_strategy() -> impl Strategy<Value = String> {
        "[a-zA-Z_][a-zA-Z0-9_]{0,10}".prop_map(|s| s.to_string())
    }

    fn path_strategy() -> impl Strategy<Value = String> {
        prop::collection::vec(key_strategy(), 1..5).prop_map(|keys| keys.join("."))
    }

    fn yaml_value_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            r#""hello|world|test|value""#.prop_map(|s| format!(r#""{}""#, s)),
            r#"'foo|bar|baz'"#.prop_map(|s| s.to_string()),
            r"(true|false)".prop_map(|s| s.to_string()),
            r"(0|1|42|100|999)".prop_map(|s| s.to_string()),
            r"(1\.5|3\.14|0\.0)".prop_map(|s| s.to_string()),
        ]
    }

    proptest! {
        #[test]
        fn prop_parse_path_never_panics(path in path_strategy()) {
            let _ = parse_path(&path);
        }

        #[test]
        fn prop_parse_assignment_with_valid_input_succeeds(
            path in path_strategy(),
            value in yaml_value_strategy()
        ) {
            let assignment = format!("{} = {}", path, value);
            let result = parse_assignment(assignment);
            prop_assert!(result.is_ok(), "Failed to parse: {}", path);
        }

        #[test]
        fn prop_parse_path_items_are_valid(path in path_strategy()) {
            let items = parse_path(&path);
            prop_assert!(!items.is_empty() || path.is_empty());
            for item in items {
                match item {
                    PathItem::Key(k) => prop_assert!(!k.is_empty()),
                    PathItem::KeyIndex(k, _) => prop_assert!(!k.is_empty()),
                }
            }
        }

        #[test]
        fn prop_build_yaml_produces_valid_output(
            assignments in prop::collection::vec(
                (path_strategy(), yaml_value_strategy()),
                0..10
            )
        ) {
            let lines: Vec<String> = assignments
                .iter()
                .map(|(path, value)| format!("{} = {}", path, value))
                .collect();
            let result = build_traefik_file_yaml(lines);
            prop_assert!(result.is_ok(), "Failed to build YAML");

            let yaml_str = result.unwrap();
            let parsed = serde_yaml::from_str::<Value>(&yaml_str);
            prop_assert!(parsed.is_ok(), "Output is not valid YAML");
        }

        #[test]
        fn prop_index_parsing_never_panics(idx_str in r"[0-9]{1,3}") {
            let path = format!("key[{}]", idx_str);
            let items = parse_path(&path);
            prop_assert!(items.len() == 1);
            if let PathItem::KeyIndex(_, idx) = &items[0] {
                prop_assert!(idx >= &0);
            }
        }

        #[test]
        fn prop_sparse_array_fills_with_nulls(sparse_idx in 0usize..10) {
            let assignment = format!("items[{}] = \"value\"", sparse_idx);
            let (path, value) = parse_assignment(assignment).unwrap();
            let mut root = Value::Mapping(serde_yaml::Mapping::new());
            insert(&mut root, &path, value);

            if let Value::Mapping(map) = &root {
                let key = Value::String("items".to_string());
                if let Some(Value::Sequence(seq)) = map.get(&key) {
                    prop_assert_eq!(seq.len(), sparse_idx + 1);
                    for (i, item) in seq.iter().enumerate() {
                        if i < sparse_idx {
                            prop_assert_eq!(item, &Value::Null);
                        }
                    }
                }
            }
        }

        #[test]
        fn prop_parse_path_with_empty_components(s in "a(\\.a){0,3}") {
            let items = parse_path(&s);
            for item in items {
                match item {
                    PathItem::Key(k) => prop_assert!(!k.is_empty()),
                    PathItem::KeyIndex(k, _) => prop_assert!(!k.is_empty()),
                }
            }
        }

        #[test]
        fn prop_traefik_unwrap_idempotent(
            assignments in prop::collection::vec(
                (path_strategy(), yaml_value_strategy()),
                1..5
            )
        ) {
            let lines: Vec<String> = assignments
                .iter()
                .map(|(path, value)| format!("traefik.{} = {}", path, value))
                .collect();

            let yaml = build_traefik_file_yaml(lines).unwrap();

            let parsed = serde_yaml::from_str::<Value>(&yaml).unwrap();
            if let Value::Mapping(map) = parsed {
                let traefik_key = Value::String("traefik".to_string());
                prop_assert!(!map.contains_key(&traefik_key));
            }
        }

        #[test]
        fn prop_value_deserialization_never_panics(raw_value in r#"[a-zA-Z0-9 ]*"#) {
            let line = format!("key = \"{}\"", raw_value);
            let result = parse_assignment(line);
            let _ = result;
        }

        #[test]
        fn prop_quoted_string_handling(
            inner in r#"[a-zA-Z0-9 ]+"#
        ) {
            let double_quoted = format!(r#"key = "{}""#, inner);
            let single_quoted = format!(r#"key = '{}'"#, inner);

            let result1 = parse_assignment(double_quoted);
            let result2 = parse_assignment(single_quoted);

            prop_assert!(result1.is_ok());
            prop_assert!(result2.is_ok());
        }
    }
}
