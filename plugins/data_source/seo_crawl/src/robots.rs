use std::collections::HashMap;

#[derive(Clone, Debug, Default)]
pub struct RobotsRules {
    pub allow: Vec<String>,
    pub disallow: Vec<String>,
    pub crawl_delay_secs: Option<f64>,
    pub sitemap_urls: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ParsedRobots {
    pub rules_by_agent: HashMap<String, RobotsRules>,
    pub sitemap_urls: Vec<String>,
    pub raw_body: String,
}

pub fn parse_robots_txt(body: &str) -> ParsedRobots {
    let mut rules_by_agent: HashMap<String, RobotsRules> = HashMap::new();
    let mut sitemap_urls = Vec::new();
    let mut current_agent = "*".to_string();

    for line in body.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "user-agent" => {
                current_agent = value.to_ascii_lowercase();
                rules_by_agent.entry(current_agent.clone()).or_default();
            }
            "allow" => {
                rules_by_agent
                    .entry(current_agent.clone())
                    .or_default()
                    .allow
                    .push(value.to_string());
            }
            "disallow" => {
                rules_by_agent
                    .entry(current_agent.clone())
                    .or_default()
                    .disallow
                    .push(value.to_string());
            }
            "crawl-delay" => {
                if let Ok(delay) = value.parse::<f64>() {
                    rules_by_agent
                        .entry(current_agent.clone())
                        .or_default()
                        .crawl_delay_secs = Some(delay);
                }
            }
            "sitemap" => {
                sitemap_urls.push(value.to_string());
                rules_by_agent
                    .entry(current_agent.clone())
                    .or_default()
                    .sitemap_urls
                    .push(value.to_string());
            }
            _ => {}
        }
    }

    ParsedRobots {
        rules_by_agent,
        sitemap_urls,
        raw_body: body.to_string(),
    }
}

pub fn is_allowed(path: &str, rules: &RobotsRules) -> bool {
    if rules.disallow.is_empty() && rules.allow.is_empty() {
        return true;
    }
    let path = if path.is_empty() { "/" } else { path };
    let disallow_match = rules
        .disallow
        .iter()
        .filter(|p| !p.is_empty())
        .any(|p| path.starts_with(p));
    if !disallow_match {
        return true;
    }
    rules
        .allow
        .iter()
        .any(|p| !p.is_empty() && path.starts_with(p))
}

pub fn path_allowed(path: &str, parsed: &ParsedRobots, user_agent: &str) -> bool {
    let ua = user_agent.to_ascii_lowercase();
    if let Some(rules) = parsed.rules_by_agent.get(&ua) {
        return is_allowed(path, rules);
    }
    if let Some(rules) = parsed.rules_by_agent.get("*") {
        return is_allowed(path, rules);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sitemap_and_disallow() {
        let body = "User-agent: *\nDisallow: /private\nSitemap: https://example.com/sitemap.xml\n";
        let parsed = parse_robots_txt(body);
        assert_eq!(parsed.sitemap_urls.len(), 1);
        assert!(!path_allowed("/private/page", &parsed, "SkipprSeoCrawl/1.0"));
        assert!(path_allowed("/public", &parsed, "SkipprSeoCrawl/1.0"));
    }
}
