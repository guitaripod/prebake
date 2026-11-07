use anyhow::{Result, bail};
use std::ops::Range;

/// A parsed Valve KeyValues text document (`.vdf`, `.acf`) that remembers where every
/// token came from, so a single value can be changed without rewriting the rest of the file.
#[derive(Debug, Clone)]
pub struct Document {
    pub root: Value,
}

#[derive(Debug, Clone)]
pub struct Pair {
    pub key: String,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub enum Value {
    Str { text: String, span: Range<usize> },
    Obj { pairs: Vec<Pair>, close: Option<usize> },
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Text(String, Range<usize>),
    Open(usize),
    Close(usize),
}

impl Document {
    pub fn parse(src: &str) -> Result<Self> {
        let tokens = tokenize(src)?;
        let mut pos = 0;
        let pairs = parse_pairs(&tokens, &mut pos, false)?;
        Ok(Self {
            root: Value::Obj { pairs, close: None },
        })
    }

    pub fn path(&self, keys: &[&str]) -> Option<&Value> {
        self.root.path(keys)
    }
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj { pairs, .. } => pairs.iter().rev().find(|p| p.key.eq_ignore_ascii_case(key)).map(|p| &p.value),
            Value::Str { .. } => None,
        }
    }

    pub fn path(&self, keys: &[&str]) -> Option<&Value> {
        keys.iter().try_fold(self, |value, key| value.get(key))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str { text, .. } => Some(text),
            Value::Obj { .. } => None,
        }
    }

    pub fn pairs(&self) -> &[Pair] {
        match self {
            Value::Obj { pairs, .. } => pairs,
            Value::Str { .. } => &[],
        }
    }
}

fn tokenize(src: &str) -> Result<Vec<Token>> {
    let bytes = src.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'{' => {
                tokens.push(Token::Open(i));
                i += 1;
            }
            b'}' => {
                tokens.push(Token::Close(i));
                i += 1;
            }
            b'[' => {
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                i += 1;
            }
            b'"' => {
                let start = i;
                i += 1;
                let mut text = Vec::new();
                loop {
                    match bytes.get(i) {
                        None => bail!("unterminated string at byte {start}"),
                        Some(b'"') => {
                            i += 1;
                            break;
                        }
                        Some(b'\\') => {
                            let escaped = match bytes.get(i + 1) {
                                Some(b'n') => b'\n',
                                Some(b't') => b'\t',
                                Some(b'\\') => b'\\',
                                Some(b'"') => b'"',
                                Some(other) => {
                                    text.push(b'\\');
                                    *other
                                }
                                None => bail!("unterminated escape at byte {i}"),
                            };
                            text.push(escaped);
                            i += 2;
                        }
                        Some(&c) => {
                            text.push(c);
                            i += 1;
                        }
                    }
                }
                tokens.push(Token::Text(String::from_utf8_lossy(&text).into_owned(), start..i));
            }
            _ => {
                let start = i;
                while i < bytes.len() && !matches!(bytes[i], b' ' | b'\t' | b'\r' | b'\n' | b'{' | b'}' | b'"') {
                    i += 1;
                }
                tokens.push(Token::Text(src[start..i].to_string(), start..i));
            }
        }
    }
    Ok(tokens)
}

fn parse_pairs(tokens: &[Token], pos: &mut usize, nested: bool) -> Result<Vec<Pair>> {
    let mut pairs = Vec::new();
    loop {
        match tokens.get(*pos) {
            None if nested => bail!("missing closing brace"),
            None => return Ok(pairs),
            Some(Token::Close(_)) if nested => return Ok(pairs),
            Some(Token::Close(at)) => bail!("unexpected closing brace at byte {at}"),
            Some(Token::Open(at)) => bail!("unexpected opening brace at byte {at}"),
            Some(Token::Text(key, _)) => {
                *pos += 1;
                let value = match tokens.get(*pos) {
                    Some(Token::Text(text, span)) => {
                        *pos += 1;
                        Value::Str {
                            text: text.clone(),
                            span: span.clone(),
                        }
                    }
                    Some(Token::Open(_)) => {
                        *pos += 1;
                        let inner = parse_pairs(tokens, pos, true)?;
                        let Some(Token::Close(close)) = tokens.get(*pos) else {
                            bail!("missing closing brace for {key}");
                        };
                        *pos += 1;
                        Value::Obj {
                            pairs: inner,
                            close: Some(*close),
                        }
                    }
                    _ => bail!("key {key} has no value"),
                };
                pairs.push(Pair { key: key.clone(), value });
            }
        }
    }
}

/// Returns `src` with the string at `path` set to `value`, creating missing objects on the way.
/// Everything outside the changed or inserted text stays byte-for-byte the same.
pub fn set_string(src: &str, path: &[&str], value: &str) -> Result<String> {
    let doc = Document::parse(src)?;
    let Some((last, parents)) = path.split_last() else {
        bail!("empty path");
    };
    let mut node = &doc.root;
    let mut depth = 0;
    for (index, key) in parents.iter().enumerate() {
        match node.get(key) {
            Some(child @ Value::Obj { .. }) => {
                node = child;
                depth += 1;
            }
            Some(Value::Str { .. }) => bail!("{key} is a value, not a section"),
            None => {
                let snippet = nested_snippet(&parents[index..], last, value, depth);
                return Ok(insert_into(src, node, &snippet));
            }
        }
    }
    match node.get(last) {
        Some(Value::Str { span, .. }) => {
            let mut out = String::with_capacity(src.len() + value.len());
            out.push_str(&src[..span.start]);
            out.push_str(&quote(value));
            out.push_str(&src[span.end..]);
            Ok(out)
        }
        Some(Value::Obj { .. }) => bail!("{last} is a section, not a value"),
        None => {
            let snippet = format!("{}{}\t\t{}\n", tabs(depth), quote(last), quote(value));
            Ok(insert_into(src, node, &snippet))
        }
    }
}

fn insert_into(src: &str, parent: &Value, snippet: &str) -> String {
    let Value::Obj { close, .. } = parent else {
        unreachable!("insert_into is only called with sections");
    };
    let at = match close {
        Some(close) => src[..*close].rfind('\n').map_or(*close, |nl| nl + 1),
        None => src.len(),
    };
    let mut out = String::with_capacity(src.len() + snippet.len() + 1);
    out.push_str(&src[..at]);
    if close.is_none() && !src.is_empty() && !src.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(snippet);
    out.push_str(&src[at..]);
    out
}

fn nested_snippet(sections: &[&str], key: &str, value: &str, depth: usize) -> String {
    let mut text = String::new();
    for (offset, section) in sections.iter().enumerate() {
        let indent = tabs(depth + offset);
        text.push_str(&format!("{indent}{}\n{indent}{{\n", quote(section)));
    }
    text.push_str(&format!("{}{}\t\t{}\n", tabs(depth + sections.len()), quote(key), quote(value)));
    for offset in (0..sections.len()).rev() {
        text.push_str(&format!("{}}}\n", tabs(depth + offset)));
    }
    text
}

fn tabs(depth: usize) -> String {
    "\t".repeat(depth)
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "\"InstallConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"ShaderCacheManager\"\n\t\t\t\t{\n\t\t\t\t\t\"HasCurrentBucket\"\t\t\"1\"\n\t\t\t\t\t\"ProcessingQueue\"\t\t\"730;2322010;\"\n\t\t\t\t}\n\t\t\t\t\"CompatToolMapping\"\n\t\t\t\t{\n\t\t\t\t\t\"2322010\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"name\"\t\t\"GE-Proton11-12-x86_64\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n";

    #[test]
    fn reads_nested_values_case_insensitively() {
        let doc = Document::parse(CONFIG).unwrap();
        let queue = doc.path(&[
            "installconfigstore",
            "software",
            "valve",
            "steam",
            "ShaderCacheManager",
            "ProcessingQueue",
        ]);
        assert_eq!(queue.and_then(Value::as_str), Some("730;2322010;"));
        let tool = doc.path(&[
            "InstallConfigStore",
            "Software",
            "Valve",
            "Steam",
            "CompatToolMapping",
            "2322010",
            "name",
        ]);
        assert_eq!(tool.and_then(Value::as_str), Some("GE-Proton11-12-x86_64"));
    }

    #[test]
    fn handles_escapes_comments_conditionals_and_unquoted_tokens() {
        let src = "// header\n\"a\"\n{\n\t\"path\"\t\t\"fozpipelinesv6\\\\steam_pipeline_cache.foz\"\n\tbare value [$LINUX]\n\t\"q\"\t\"say \\\"hi\\\"\"\n}\n";
        let doc = Document::parse(src).unwrap();
        assert_eq!(
            doc.path(&["a", "path"]).and_then(Value::as_str),
            Some("fozpipelinesv6\\steam_pipeline_cache.foz")
        );
        assert_eq!(doc.path(&["a", "bare"]).and_then(Value::as_str), Some("value"));
        assert_eq!(doc.path(&["a", "q"]).and_then(Value::as_str), Some("say \"hi\""));
    }

    #[test]
    fn rejects_unbalanced_documents() {
        assert!(Document::parse("\"a\"\n{\n\t\"b\"\t\"c\"\n").is_err());
        assert!(Document::parse("\"a\"\t\"b\"\n}\n").is_err());
        assert!(Document::parse("\"a\"\t\"unterminated\n").is_err());
    }

    #[test]
    fn replaces_an_existing_value_in_place() {
        let path = [
            "InstallConfigStore",
            "Software",
            "Valve",
            "Steam",
            "ShaderCacheManager",
            "HasCurrentBucket",
        ];
        let out = set_string(CONFIG, &path, "0").unwrap();
        assert_eq!(
            out,
            CONFIG.replace("\"HasCurrentBucket\"\t\t\"1\"", "\"HasCurrentBucket\"\t\t\"0\"")
        );
    }

    #[test]
    fn inserts_a_missing_value_before_the_closing_brace_with_matching_indent() {
        let path = [
            "InstallConfigStore",
            "Software",
            "Valve",
            "Steam",
            "ShaderCacheManager",
            "EnableShaderBackgroundProcessing",
        ];
        let out = set_string(CONFIG, &path, "1").unwrap();
        let doc = Document::parse(&out).unwrap();
        assert_eq!(doc.path(&path).and_then(Value::as_str), Some("1"));
        assert!(out.contains(
            "\t\t\t\t\t\"ProcessingQueue\"\t\t\"730;2322010;\"\n\t\t\t\t\t\"EnableShaderBackgroundProcessing\"\t\t\"1\"\n\t\t\t\t}\n"
        ));
        assert_eq!(
            out.len(),
            CONFIG.len() + "\t\t\t\t\t\"EnableShaderBackgroundProcessing\"\t\t\"1\"\n".len()
        );
    }

    #[test]
    fn creates_missing_sections() {
        let path = ["InstallConfigStore", "Software", "Valve", "Steam", "Fresh", "Inner", "Key"];
        let out = set_string(CONFIG, &path, "v").unwrap();
        let doc = Document::parse(&out).unwrap();
        assert_eq!(doc.path(&path).and_then(Value::as_str), Some("v"));
        assert!(out.contains(
            "\t\t\t\t\"Fresh\"\n\t\t\t\t{\n\t\t\t\t\t\"Inner\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"Key\"\t\t\"v\"\n\t\t\t\t\t}\n\t\t\t\t}\n"
        ));
    }

    #[test]
    fn untouched_documents_round_trip() {
        let path = [
            "InstallConfigStore",
            "Software",
            "Valve",
            "Steam",
            "ShaderCacheManager",
            "ProcessingQueue",
        ];
        let out = set_string(CONFIG, &path, "730;2322010;").unwrap();
        assert_eq!(out, CONFIG);
    }
}
