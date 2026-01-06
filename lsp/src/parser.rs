use serde_json::Value;
use tower_lsp::lsp_types::Position;

/// A dependency found in package.json
#[derive(Debug, Clone)]
pub struct Dependency {
    /// Package name (e.g., "express")
    pub name: String,
    /// Version specifier (e.g., "^4.18.0")
    pub version: String,
    /// Cleaned version for semver comparison (e.g., "4.18.0")
    pub clean_version: String,
    /// Position of the version string in the document
    pub version_position: Position,
    /// Line containing this dependency
    pub line: u32,
    /// Start column of the package name (excluding quotes)
    pub name_start_col: u32,
    /// End column of the package name (excluding quotes)
    pub name_end_col: u32,
    /// Start column of the version value (excluding quotes)
    pub version_start_col: u32,
    /// End column of the version value (excluding quotes)
    pub version_end_col: u32,
    /// Type of dependency section
    pub dep_type: DependencyType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyType {
    Dependencies,
    DevDependencies,
    PeerDependencies,
    OptionalDependencies,
}

impl DependencyType {
    pub fn as_str(&self) -> &'static str {
        match self {
            DependencyType::Dependencies => "dependencies",
            DependencyType::DevDependencies => "devDependencies",
            DependencyType::PeerDependencies => "peerDependencies",
            DependencyType::OptionalDependencies => "optionalDependencies",
        }
    }
}

/// Parse package.json text and extract all dependencies with their positions
pub fn parse_package_json(text: &str) -> Vec<Dependency> {
    let mut dependencies = Vec::new();

    // Parse JSON to get structure
    let json: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return dependencies,
    };

    let dep_types = [
        ("dependencies", DependencyType::Dependencies),
        ("devDependencies", DependencyType::DevDependencies),
        ("peerDependencies", DependencyType::PeerDependencies),
        ("optionalDependencies", DependencyType::OptionalDependencies),
    ];

    for (section_name, dep_type) in dep_types {
        if let Some(deps) = json.get(section_name).and_then(|d| d.as_object()) {
            for (name, version_value) in deps {
                if let Some(version) = version_value.as_str() {
                    // Find the position in the text
                    if let Some(dep) = find_dependency_position(text, section_name, name, version, dep_type) {
                        dependencies.push(dep);
                    }
                }
            }
        }
    }

    dependencies
}

/// Find the position of a specific dependency in the text
fn find_dependency_position(
    text: &str,
    section: &str,
    name: &str,
    version: &str,
    dep_type: DependencyType,
) -> Option<Dependency> {
    let lines: Vec<&str> = text.lines().collect();
    
    // Find the section start
    let section_pattern = format!("\"{}\"", section);
    let mut in_section = false;
    let mut brace_depth = 0;
    
    for (line_idx, line) in lines.iter().enumerate() {
        // Check if we're entering the target section
        if !in_section && line.contains(&section_pattern) {
            in_section = true;
            if line.contains('{') {
                brace_depth = 1;
            }
            continue;
        }
        
        if in_section {
            // Track braces
            for ch in line.chars() {
                match ch {
                    '{' => brace_depth += 1,
                    '}' => {
                        brace_depth -= 1;
                        if brace_depth == 0 {
                            in_section = false;
                        }
                    }
                    _ => {}
                }
            }
            
            if brace_depth == 0 {
                continue;
            }
            
            // Look for the package name in this line
            let name_pattern = format!("\"{}\"", name);
            if let Some(name_pos) = line.find(&name_pattern) {
                // Calculate name column positions (inside quotes)
                let name_start = name_pos + 1; // +1 for opening quote
                let name_end = name_start + name.len();
                
                // Find the version string after the name
                let after_name = &line[name_pos + name_pattern.len()..];
                
                // Look for the version value
                let version_pattern = format!("\"{}\"", version);
                if let Some(version_offset) = after_name.find(&version_pattern) {
                    let version_start = name_pos + name_pattern.len() + version_offset + 1; // +1 for opening quote
                    let version_end = version_start + version.len();
                    
                    return Some(Dependency {
                        name: name.to_string(),
                        version: version.to_string(),
                        clean_version: clean_version(version),
                        version_position: Position {
                            line: line_idx as u32,
                            character: version_start as u32,
                        },
                        line: line_idx as u32,
                        name_start_col: name_start as u32,
                        name_end_col: name_end as u32,
                        version_start_col: version_start as u32,
                        version_end_col: version_end as u32,
                        dep_type,
                    });
                }
            }
        }
    }
    
    None
}

/// Clean version string for semver parsing
/// Removes prefixes like ^, ~, >=, etc.
pub fn clean_version(version: &str) -> String {
    let version = version.trim();
    
    // Handle special versions
    if version == "*" || version == "latest" || version.starts_with("git") || version.starts_with("http") || version.starts_with("file:") {
        return String::new();
    }
    
    // Remove common prefixes
    let cleaned = version
        .trim_start_matches('^')
        .trim_start_matches('~')
        .trim_start_matches(">=")
        .trim_start_matches("<=")
        .trim_start_matches('>')
        .trim_start_matches('<')
        .trim_start_matches('=')
        .trim_start_matches('v');
    
    // Handle ranges like "1.0.0 - 2.0.0"
    if let Some(idx) = cleaned.find(" - ") {
        return cleaned[..idx].trim().to_string();
    }
    
    // Handle "||" ranges, take the last one
    if let Some(idx) = cleaned.rfind("||") {
        let last_part = cleaned[idx + 2..].trim();
        return clean_version(last_part);
    }
    
    // Handle "x" ranges like "1.x" or "1.2.x"
    let cleaned = cleaned.replace(".x", ".0").replace(".*", ".0");
    
    cleaned.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_version() {
        assert_eq!(clean_version("^1.2.3"), "1.2.3");
        assert_eq!(clean_version("~1.2.3"), "1.2.3");
        assert_eq!(clean_version(">=1.2.3"), "1.2.3");
        assert_eq!(clean_version("1.2.3"), "1.2.3");
        assert_eq!(clean_version("v1.2.3"), "1.2.3");
    }

    #[test]
    fn test_parse_package_json() {
        let text = r#"{
  "name": "test",
  "dependencies": {
    "express": "^4.18.0",
    "lodash": "~4.17.21"
  },
  "devDependencies": {
    "typescript": "^5.0.0"
  }
}"#;
        
        let deps = parse_package_json(text);
        assert_eq!(deps.len(), 3);
        
        let express = deps.iter().find(|d| d.name == "express").unwrap();
        assert_eq!(express.version, "^4.18.0");
        assert_eq!(express.clean_version, "4.18.0");
    }
}

