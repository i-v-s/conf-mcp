use anyhow::{Context, Result, bail};
use htmd::{
    Element, HtmlToMarkdown,
    element_handler::{HandlerResult, Handlers},
    options::{BulletListMarker, Options},
};
use html5ever::{Attribute, QualName, parse_document, tendril::TendrilSink};
use markup5ever_rcdom::{Node, NodeData, RcDom};
use std::cell::RefCell;
use std::rc::Rc;
use url::Url;

pub fn convert(html: &str, page_url: &Url) -> Result<String> {
    let converter = HtmlToMarkdown::builder()
        .options(Options {
            bullet_list_marker: BulletListMarker::Dash,
            ul_bullet_spacing: 1,
            ol_number_spacing: 1,
            preformatted_code: true,
            ..Options::default()
        })
        .skip_tags(vec!["script", "style", "noscript", "head", "iframe"])
        .add_handler(vec!["br"], line_break)
        .build();
    let tree = parse_document(RcDom::default(), Default::default())
        .one(html)
        .document;
    prepare_tree(&tree, page_url)?;
    Ok(converter
        .convert(&tree)
        .context("Не удалось преобразовать HTML в Markdown")?
        .trim()
        .to_owned())
}

fn prepare_tree(root: &Rc<Node>, page_url: &Url) -> Result<()> {
    let mut pending = vec![(root.clone(), 0)];
    while let Some((node, depth)) = pending.pop() {
        if depth >= 128 {
            bail!("HTML страницы имеет слишком большую глубину вложенности");
        }
        if let NodeData::Element { name, attrs, .. } = &node.data {
            let mut attrs = attrs.borrow_mut();
            attrs.retain_mut(|attr| {
                let is_link = &*name.local == "a" && &*attr.name.local == "href";
                let is_image =
                    &*name.local == "img" && matches!(attr.name.local.as_ref(), "src" | "href");
                if !is_link && !is_image {
                    return true;
                }
                match page_url.join(&attr.value) {
                    Ok(url)
                        if matches!(url.scheme(), "http" | "https")
                            || (is_link && matches!(url.scheme(), "mailto" | "tel")) =>
                    {
                        attr.value = url.as_str().into();
                        true
                    }
                    _ => false,
                }
            });
            if &*name.local == "pre" {
                // Confluence задаёт язык через brush и часто возвращает pre без code.
                if let Some(language) = brush_language(&attrs) {
                    if let Some(class) = attrs.iter_mut().find(|attr| &*attr.name.local == "class")
                    {
                        if !class
                            .value
                            .split_whitespace()
                            .any(|value| value.starts_with("language-"))
                        {
                            class.value = format!("{} language-{language}", class.value).into();
                        }
                    } else {
                        attrs.push(Attribute {
                            name: QualName::new(None, "".into(), "class".into()),
                            value: format!("language-{language}").into(),
                        });
                    }
                }
                let has_code = node.children.borrow().iter().any(|child| matches!(&child.data, NodeData::Element { name, .. } if &*name.local == "code"));
                if !has_code {
                    let code = new_element(&node, name, "code");
                    for child in node.children.borrow_mut().drain(..) {
                        child.parent.set(Some(Rc::downgrade(&code)));
                        code.children.borrow_mut().push(child);
                    }
                    node.children.borrow_mut().push(code);
                }
            } else if &*name.local == "table" {
                add_empty_header(&node, name);
            }
        }
        pending.extend(
            node.children
                .borrow()
                .iter()
                .map(|child| (child.clone(), depth + 1)),
        );
    }
    Ok(())
}

fn brush_language(attrs: &[Attribute]) -> Option<String> {
    attrs
        .iter()
        .find(|attr| &*attr.name.local == "data-syntaxhighlighter-params")?
        .value
        .split(';')
        .find_map(|param| param.trim().strip_prefix("brush:"))
        .map(str::trim)
        .filter(|language| {
            !language.is_empty()
                && language
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+'))
        })
        .map(str::to_owned)
}

fn line_break(handlers: &dyn Handlers, element: &Element<'_>) -> Option<HandlerResult> {
    let mut ancestor = element.node.parent();
    while let Some(node) = ancestor {
        match node.tag_name() {
            Some("pre") => return Some("\n".into()),
            Some("td" | "th") => return Some("<br>".into()),
            _ => {}
        }
        ancestor = node.parent();
    }
    handlers.fallback(element)
}

fn new_element(parent: &Rc<Node>, name: &QualName, tag: &str) -> Rc<Node> {
    let mut name = name.clone();
    name.local = tag.into();
    let node = Node::new(NodeData::Element {
        name,
        attrs: RefCell::new(vec![]),
        template_contents: RefCell::new(None),
        mathml_annotation_xml_integration_point: false,
    });
    node.parent.set(Some(Rc::downgrade(parent)));
    node
}

fn add_empty_header(table: &Rc<Node>, name: &QualName) {
    let mut pending = table.children.borrow().clone();
    let mut columns = 0;
    while let Some(node) = pending.pop() {
        if let NodeData::Element { name, .. } = &node.data {
            match name.local.as_ref() {
                "thead" => return,
                "tbody" | "tfoot" => pending.extend(node.children.borrow().iter().cloned()),
                "tr" => {
                    let mut count = 0;
                    for child in node.children.borrow().iter() {
                        if let NodeData::Element { name, .. } = &child.data {
                            match name.local.as_ref() {
                                "th" => return,
                                "td" => count += 1,
                                _ => {}
                            }
                        }
                    }
                    columns = columns.max(count);
                }
                _ => {}
            }
        }
    }
    if columns > 0 {
        // У GFM-таблицы обязателен заголовок; пустой ряд сохраняет все строки данных.
        let head = new_element(table, name, "thead");
        let row = new_element(&head, name, "tr");
        row.children
            .borrow_mut()
            .extend((0..columns).map(|_| new_element(&row, name, "th")));
        head.children.borrow_mut().push(row);
        table.children.borrow_mut().insert(0, head);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn markdown(html: &str) -> String {
        convert(
            html,
            &Url::parse("https://wiki.test/confluence/display/DEV/Page").unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn headings_formatting_entities_and_links() {
        let result = markdown(
            r##"<h2>Привет &amp; Rust</h2><p>A&nbsp;<strong>B</strong> <em>C</em> &copy; &#x2713;<br>line</p><p><a href="../../pages/viewpage.action?pageId=42&amp;src=link">Page</a><a href="#anchor">Section</a><img src="/confluence/download/image.png" alt="Diagram"></p><script>secret script</script><style>secret style</style>"##,
        );
        let result = result.replace("\\&", "&");
        assert!(result.contains("## Привет & Rust"), "{result}");
        assert!(result.contains("A\u{a0}**B** *C* © ✓  \nline"), "{result}");
        assert!(
            result.contains(
                "[Page](https://wiki.test/confluence/pages/viewpage.action?pageId=42&src=link)"
            ),
            "{result}"
        );
        assert!(result.contains("[Section](https://wiki.test/confluence/display/DEV/Page#anchor)"));
        assert!(result.contains("![Diagram](https://wiki.test/confluence/download/image.png)"));
        assert!(!result.contains("secret"));
    }

    #[test]
    fn code_preserves_whitespace_and_uses_safe_fences() {
        let result = markdown(
            "<pre data-syntaxhighlighter-params='brush: rust; gutter: false'><code>fn main() {\n    println!(&quot;```&quot;);\n}\n</code></pre><p><code>a`b</code></p>",
        );
        assert!(
            result.contains("````rust\nfn main() {\n    println!(\"```\");\n}\n````"),
            "{result}"
        );
        assert!(result.contains("``a`b``"));
    }

    #[test]
    fn lists_tables_and_quoted_blocks() {
        let result = markdown(
            "<ol start='3'><li>one<ul><li>nested</li></ul></li><li>two</li></ol><table><tbody><tr><th>Name</th><th>Value</th></tr><tr><td>A|B</td><td>1<br/>2</td></tr></tbody></table><blockquote><p>Quote</p></blockquote>",
        );
        assert!(result.contains("3. one\n   - nested\n4. two"), "{result}");
        let rows = result
            .lines()
            .filter(|line| line.starts_with('|'))
            .map(|line| {
                line.replace("\\|", "&#124;")
                    .trim_matches('|')
                    .split('|')
                    .map(|cell| cell.trim().to_owned())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 3, "{result}");
        assert_eq!(rows[0], ["Name", "Value"]);
        assert!(
            rows[1]
                .iter()
                .all(|cell| !cell.is_empty() && cell.chars().all(|c| c == '-'))
        );
        assert_eq!(rows[2], ["A&#124;B", "1<br>2"]);
        assert!(result.contains("> Quote"));
        let no_header = markdown(
            "<table><tr><td>first data row</td><td>1</td></tr><tr><td>second data row</td><td>2</td></tr></table>",
        );
        assert_eq!(
            no_header
                .lines()
                .filter(|line| line.starts_with('|'))
                .count(),
            4,
            "{no_header}"
        );
        assert!(
            no_header.contains("first data row") && no_header.contains("second data row"),
            "{no_header}"
        );
    }

    #[test]
    fn empty_content_and_unclosed_html_are_handled() {
        assert_eq!(markdown(""), "");
        assert_eq!(
            markdown("<p>Text<img src='/image.png'><br>More</p>"),
            "Text![](https://wiki.test/image.png)  \nMore"
        );
        assert_eq!(markdown("<div><p>unclosed"), "unclosed");
        assert_eq!(markdown("<a href='javascript:alert(1)'>link</a>"), "link");
        assert_eq!(markdown("<p>A<strong> B </strong>C</p>"), "A **B** C");
    }

    #[test]
    fn confluence_pre_without_code_keeps_language_and_indentation() {
        let result = markdown(
            "<pre class='syntaxhighlighter-pre' data-syntaxhighlighter-params='brush: python; gutter: false'>if ready:\n    emit(&quot;ok&quot;)<br>    done()</pre>",
        );
        assert_eq!(
            result,
            "```python\nif ready:\n    emit(\"ok\")\n    done()\n```"
        );
    }

    #[test]
    fn html5_parser_repairs_omitted_list_and_table_end_tags() {
        let result = markdown("<ul><li>one<li>two</ul><table><tr><th>A<th>B<tr><td>x<td>y</table>");
        assert!(result.contains("- one\n- two"), "{result}");
        assert!(
            result.contains("| A | B |") && result.contains("| x | y |"),
            "{result}"
        );
    }
}
