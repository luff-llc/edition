//! Versions, tag templates and version-file patterns.

use std::cmp::Ordering;
use std::fmt;

/// `MAJOR.MINOR.PATCH` with an optional pre-release (`-dev`, `-rc.1`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

impl Version {
    pub fn parse(s: &str) -> Result<Version, String> {
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let nums: Vec<&str> = core.split('.').collect();
        if nums.len() != 3 {
            return Err(format!("`{s}` is not a MAJOR.MINOR.PATCH version"));
        }
        let n = |x: &str| -> Result<u64, String> {
            if x.is_empty() || !x.bytes().all(|b| b.is_ascii_digit()) || (x.len() > 1 && x.starts_with('0')) {
                return Err(format!("`{s}` is not a MAJOR.MINOR.PATCH version"));
            }
            x.parse::<u64>().map_err(|_| format!("`{s}` has a number that is too large"))
        };
        let pre = match pre {
            Some(p) if p.is_empty() || !p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') => {
                return Err(format!("`{s}` has an invalid pre-release"));
            }
            Some(p) => Some(p.to_string()),
            None => None,
        };
        Ok(Version { major: n(nums[0])?, minor: n(nums[1])?, patch: n(nums[2])?, pre })
    }

    /// `X.Y`, the release line this version belongs to.
    pub fn line(&self) -> String {
        format!("{}.{}", self.major, self.minor)
    }

    pub fn base(&self) -> Version {
        Version { pre: None, ..self.clone() }
    }

    pub fn is_final(&self) -> bool {
        self.pre.is_none()
    }

    pub fn with_pre(&self, pre: &str) -> Version {
        Version { pre: Some(pre.to_string()), ..self.clone() }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(p) = &self.pre {
            write!(f, "-{p}")?;
        }
        Ok(())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch)).then_with(|| {
            match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            }
        })
    }
}

/// A release line `X.Y`.
pub fn parse_line(s: &str) -> Result<(u64, u64), String> {
    let v = Version::parse(&format!("{s}.0")).map_err(|_| format!("`{s}` is not a release line (X.Y)"))?;
    if v.pre.is_some() {
        return Err(format!("`{s}` is not a release line (X.Y)"));
    }
    Ok((v.major, v.minor))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    Lit(String),
    Var(String),
}

/// Literal text with `{name}` placeholders, such as `v{version}-rc.{n}` or `release/{line}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    source: String,
    parts: Vec<Part>,
}

impl Template {
    pub fn parse(s: &str) -> Result<Template, String> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            match c {
                '{' => {
                    let mut name = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '.' => name.push(c),
                            _ => return Err(format!("template `{s}` has an unclosed or invalid placeholder")),
                        }
                    }
                    if name.is_empty() {
                        return Err(format!("template `{s}` has an empty placeholder"));
                    }
                    if !lit.is_empty() {
                        parts.push(Part::Lit(std::mem::take(&mut lit)));
                    } else if matches!(parts.last(), Some(Part::Var(_))) {
                        return Err(format!("template `{s}` has two placeholders with nothing between them"));
                    }
                    parts.push(Part::Var(name));
                }
                '}' => return Err(format!("template `{s}` has a stray `}}`")),
                c => lit.push(c),
            }
        }
        if !lit.is_empty() {
            parts.push(Part::Lit(lit));
        }
        Ok(Template { source: s.to_string(), parts })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn vars(&self) -> Vec<&str> {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Var(v) => Some(v.as_str()),
                Part::Lit(_) => None,
            })
            .collect()
    }

    pub fn render(&self, vars: &[(&str, &str)]) -> Result<String, String> {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                Part::Lit(l) => out.push_str(l),
                Part::Var(v) => match vars.iter().find(|(k, _)| k == v) {
                    Some((_, val)) => out.push_str(val),
                    None => return Err(format!("template `{}` needs a value for {{{v}}}", self.source)),
                },
            }
        }
        Ok(out)
    }

    /// Matches the whole string; each placeholder takes the text up to the first
    /// occurrence of the literal that follows it.
    pub fn matches(&self, s: &str) -> Option<Vec<(String, String)>> {
        let mut pos = 0;
        let mut out = Vec::new();
        for (i, p) in self.parts.iter().enumerate() {
            match p {
                Part::Lit(l) => {
                    if !s[pos..].starts_with(l.as_str()) {
                        return None;
                    }
                    pos += l.len();
                }
                Part::Var(v) => {
                    let rest = &s[pos..];
                    let len = match self.parts.get(i + 1) {
                        Some(Part::Lit(next)) => {
                            let start = rest.char_indices().nth(1).map(|(i, _)| i)?;
                            start + rest[start..].find(next.as_str())?
                        }
                        _ => rest.len(),
                    };
                    if len == 0 {
                        return None;
                    }
                    out.push((v.clone(), rest[..len].to_string()));
                    pos += len;
                }
            }
        }
        (pos == s.len()).then_some(out)
    }

    pub fn get<'a>(captures: &'a [(String, String)], name: &str) -> Option<&'a str> {
        captures.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// A pattern with exactly one placeholder, found inside a line of a file:
/// `version = "{version}"` or `swift_commit={commit}`.
#[derive(Clone, Debug)]
pub struct FilePattern {
    prefix: String,
    suffix: String,
    var: String,
}

impl FilePattern {
    pub fn parse(s: &str, allowed: &[&str]) -> Result<FilePattern, String> {
        let t = Template::parse(s)?;
        let vars = t.vars();
        if vars.len() != 1 || !allowed.contains(&vars[0]) {
            return Err(format!(
                "pattern `{s}` must have exactly one placeholder, one of {}",
                allowed.iter().map(|a| format!("{{{a}}}")).collect::<Vec<_>>().join(", ")
            ));
        }
        let var = vars[0].to_string();
        let (prefix, suffix) = s.split_once(&format!("{{{var}}}")).unwrap_or((s, ""));
        Ok(FilePattern { prefix: prefix.to_string(), suffix: suffix.to_string(), var })
    }

    pub fn var(&self) -> &str {
        &self.var
    }

    fn value_char(&self, c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_' | '/')
    }

    /// Byte range of the value in `line`, if the pattern occurs in it.
    pub fn find_in_line(&self, line: &str) -> Option<(usize, usize)> {
        let mut from = 0;
        while let Some(i) = line[from..].find(self.prefix.as_str()) {
            let start = from + i + self.prefix.len();
            let len: usize = line[start..].chars().take_while(|c| self.value_char(*c)).map(char::len_utf8).sum();
            if len > 0 && line[start + len..].starts_with(self.suffix.as_str()) {
                return Some((start, start + len));
            }
            let at = from + i;
            match line[at..].chars().next() {
                Some(c) => from = at + c.len_utf8(),
                None => break,
            }
        }
        None
    }

    /// Every match in `text` (after the line equal to `after`, first match only, when given).
    /// Returns `(line number, value)`.
    pub fn find_all(&self, text: &str, after: Option<&str>) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        let mut armed = after.is_none();
        for (n, line) in text.lines().enumerate() {
            if !armed {
                if Some(line.trim()) == after {
                    armed = true;
                }
                continue;
            }
            if let Some((a, b)) = self.find_in_line(line) {
                out.push((n + 1, line[a..b].to_string()));
                if after.is_some() {
                    break;
                }
            }
        }
        out
    }

    /// Replaces every value `find_all` would return with `new`.
    pub fn replace_all(&self, text: &str, after: Option<&str>, new: &str) -> (String, usize) {
        let mut out = String::with_capacity(text.len());
        let mut armed = after.is_none();
        let mut done = false;
        let mut count = 0;
        for piece in text.split_inclusive('\n') {
            let (line, nl) = match piece.strip_suffix('\n') {
                Some(l) => (l, "\n"),
                None => (piece, ""),
            };
            if !armed {
                if Some(line.trim()) == after {
                    armed = true;
                }
                out.push_str(piece);
                continue;
            }
            match (done, self.find_in_line(line)) {
                (false, Some((a, b))) => {
                    out.push_str(&line[..a]);
                    out.push_str(new);
                    out.push_str(&line[b..]);
                    out.push_str(nl);
                    count += 1;
                    if after.is_some() {
                        done = true;
                    }
                }
                _ => out.push_str(piece),
            }
        }
        (out, count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_order() {
        let v = Version::parse("0.6.0-rc.2").expect("parse");
        assert_eq!(v.line(), "0.6");
        assert_eq!(v.to_string(), "0.6.0-rc.2");
        assert!(Version::parse("0.6").is_err());
        assert!(Version::parse("01.2.3").is_err());
        assert!(Version::parse("0.6.0-").is_err());
        let dev = Version::parse("0.6.0-dev").expect("parse");
        let fin = Version::parse("0.6.0").expect("parse");
        assert!(dev < fin);
        assert!(fin < Version::parse("0.6.1").expect("parse"));
        assert_eq!(parse_line("0.6"), Ok((0, 6)));
        assert!(parse_line("0.6.1").is_err());
    }

    #[test]
    fn templates_render_and_match() {
        let t = Template::parse("v{version}-rc.{n}").expect("template");
        assert_eq!(t.render(&[("version", "0.6.0"), ("n", "3")]).expect("render"), "v0.6.0-rc.3");
        let caps = t.matches("v0.6.0-rc.3").expect("match");
        assert_eq!(Template::get(&caps, "version"), Some("0.6.0"));
        assert_eq!(Template::get(&caps, "n"), Some("3"));
        assert!(t.matches("v0.6.0").is_none());
        let f = Template::parse("v{version}").expect("template");
        assert_eq!(Template::get(&f.matches("v0.6.0-rc.1").expect("m"), "version"), Some("0.6.0-rc.1"));
        assert!(Template::parse("{a}{b}").is_err());
        assert!(Template::parse("v{version").is_err());
        assert!(t.render(&[("version", "1.0.0")]).is_err());
    }

    #[test]
    fn file_patterns_find_and_replace() {
        let p = FilePattern::parse("version = \"{version}\"", &["version"]).expect("pattern");
        let text = "[package]\nname = \"aead\"\nversion = \"0.5.2\"\n\n[workspace.package]\nversion = \"0.5.2\"\nedition = \"2021\"\n";
        let all = p.find_all(text, None);
        assert_eq!(all.len(), 2);
        let scoped = p.find_all(text, Some("[workspace.package]"));
        assert_eq!(scoped, vec![(6, "0.5.2".to_string())]);
        let (out, n) = p.replace_all(text, Some("[workspace.package]"), "0.6.0-dev");
        assert_eq!(n, 1);
        assert!(out.contains("name = \"aead\"\nversion = \"0.5.2\""));
        assert!(out.contains("[workspace.package]\nversion = \"0.6.0-dev\""));
        let pin = FilePattern::parse("=\"{version}\" x", &["version"]);
        assert!(pin.is_ok());
        let dep = FilePattern::parse("version = \"={version}\"", &["version"]).expect("pattern");
        let line = "inkwell-core = { path = \"../inkwell-core\", version = \"=0.5.2\", features = [\"w\"] }";
        assert_eq!(dep.find_all(line, None), vec![(1, "0.5.2".to_string())]);
        let bare = FilePattern::parse("{version}", &["version"]).expect("bare pattern");
        let pre = FilePattern::parse("v={version}", &["version"]).expect("pattern");
        assert_eq!(pre.find_all("é ü v=2.0.0\n", None), vec![(1, "2.0.0".to_string())]);
        assert_eq!(bare.find_all("1.2.0\n", None), vec![(1, "1.2.0".to_string())]);
        assert!(FilePattern::parse("x={commit}", &["version"]).is_err());
    }
}
