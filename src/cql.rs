use anyhow::{Result, bail};

pub fn build(query: Option<&str>, cql: Option<&str>) -> Result<String> {
    match (query, cql) {
        (Some(query), None) if !query.trim().is_empty() => Ok(format!(
            "type = page AND (text ~ \"{}\")",
            query.trim().replace('\\', "\\\\").replace('"', "\\\"")
        )),
        (None, Some(cql)) if !cql.trim().is_empty() => restrict_to_pages(cql.trim()),
        _ => bail!("Укажите ровно один непустой параметр: query или cql"),
    }
}

fn restrict_to_pages(cql: &str) -> Result<String> {
    let order_at = find_order_by(cql)?;
    let (condition, order) = order_at.map_or((cql, ""), |index| cql.split_at(index));
    let condition = condition.trim();
    let filter = if condition.is_empty() {
        "type = page".to_owned()
    } else {
        format!("type = page AND ({condition})")
    };
    Ok(if order.is_empty() {
        filter
    } else {
        format!("{filter} {order}")
    })
}

fn find_order_by(cql: &str) -> Result<Option<usize>> {
    let bytes = cql.as_bytes();
    let mut index = 0;
    let mut quote = None;
    let mut depth = 0_u32;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(delimiter) = quote {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == delimiter {
                quote = None;
            }
        } else {
            match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'(' => depth += 1,
                b')' => {
                    if depth == 0 {
                        bail!("Несбалансированные скобки в CQL");
                    }
                    depth -= 1;
                }
                _ if byte.is_ascii_alphabetic() => {
                    let start = index;
                    while index < bytes.len()
                        && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                    {
                        index += 1;
                    }
                    if depth == 0 && cql[start..index].eq_ignore_ascii_case("order") {
                        let after_order = index;
                        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                            index += 1;
                        }
                        if index > after_order
                            && bytes
                                .get(index..index + 2)
                                .is_some_and(|s| s.eq_ignore_ascii_case(b"by"))
                            && bytes.get(index + 2).is_none_or(u8::is_ascii_whitespace)
                        {
                            if cql[index + 2..].trim().is_empty() {
                                bail!("После ORDER BY необходимо указать поле сортировки");
                            }
                            return Ok(Some(start));
                        }
                    }
                    continue;
                }
                _ => {}
            }
        }
        index += 1;
    }
    if quote.is_some() || depth != 0 {
        bail!("Незакрытая строка или скобка в CQL");
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_escaped_and_arguments_are_exclusive() {
        assert_eq!(
            build(Some("  Rust \"stdio\" \\ MCP  "), None).unwrap(),
            r#"type = page AND (text ~ "Rust \"stdio\" \\ MCP")"#
        );
        for (query, cql) in [
            (None, None),
            (Some(""), None),
            (Some(" \n"), None),
            (None, Some("")),
            (Some("rust"), Some("space = DEV")),
            (Some(""), Some("space = DEV")),
        ] {
            assert!(build(query, cql).is_err());
        }
    }

    #[test]
    fn cql_keeps_ordering_and_boolean_precedence() {
        assert_eq!(
            build(
                None,
                Some("space = DEV OR label = docs ORDER BY lastmodified DESC")
            )
            .unwrap(),
            "type = page AND (space = DEV OR label = docs) ORDER BY lastmodified DESC"
        );
        assert_eq!(
            build(None, Some("order\nby title ASC")).unwrap(),
            "type = page order\nby title ASC"
        );
    }

    #[test]
    fn order_by_in_strings_is_not_a_clause() {
        let cql = r#"(title = "order by résumé" OR text ~ "escaped \" order by") AND label = 'order by' oRdEr By title"#;
        assert_eq!(
            build(None, Some(cql)).unwrap(),
            r#"type = page AND ((title = "order by résumé" OR text ~ "escaped \" order by") AND label = 'order by') oRdEr By title"#
        );
        for cql in [
            "title = 'missing",
            "(space = DEV",
            "space = DEV)",
            "space = DEV ORDER BY ",
        ] {
            assert!(build(None, Some(cql)).is_err(), "{cql}");
        }
    }
}
