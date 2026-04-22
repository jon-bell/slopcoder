//! SSH port allocation for workspace pods.
//!
//! Each workspace gets a unique TCP port on `ssh.<SSH_DOMAIN>`.
//! The coordinator manages a port pool and updates the nginx TCP
//! services ConfigMap when workspaces are created/destroyed.

use std::collections::{HashMap, HashSet};

/// Manages SSH port allocation from a configured range.
#[derive(Debug)]
pub struct SshPortPool {
    range_start: u16,
    range_end: u16,
    allocated: HashMap<String, u16>, // slug -> port
    used: HashSet<u16>,
}

impl SshPortPool {
    /// Create a new port pool from a range string like "30000-32767".
    pub fn new(range: &str) -> Self {
        let (start, end) = Self::parse_range(range);
        Self {
            range_start: start,
            range_end: end,
            allocated: HashMap::new(),
            used: HashSet::new(),
        }
    }

    fn parse_range(range: &str) -> (u16, u16) {
        let parts: Vec<&str> = range.split('-').collect();
        if parts.len() == 2 {
            if let (Ok(start), Ok(end)) = (parts[0].parse::<u16>(), parts[1].parse::<u16>()) {
                if start < end {
                    return (start, end);
                }
            }
        }
        (30000, 32767) // default
    }

    /// Allocate a port for a workspace slug. Returns None if pool exhausted.
    pub fn allocate(&mut self, slug: &str) -> Option<u16> {
        if let Some(&port) = self.allocated.get(slug) {
            return Some(port);
        }
        for port in self.range_start..=self.range_end {
            if !self.used.contains(&port) {
                self.used.insert(port);
                self.allocated.insert(slug.to_string(), port);
                return Some(port);
            }
        }
        None
    }

    /// Deallocate a port for a workspace slug.
    pub fn deallocate(&mut self, slug: &str) {
        if let Some(port) = self.allocated.remove(slug) {
            self.used.remove(&port);
        }
    }

    /// Get the allocated port for a slug.
    pub fn get(&self, slug: &str) -> Option<u16> {
        self.allocated.get(slug).copied()
    }

    /// Get all current allocations (for ConfigMap generation).
    pub fn allocations(&self) -> &HashMap<String, u16> {
        &self.allocated
    }

    /// Number of available ports.
    pub fn available(&self) -> usize {
        (self.range_end - self.range_start + 1) as usize - self.used.len()
    }
}

/// Format an SSH command for display in the UI.
pub fn ssh_command(port: u16, ssh_domain: &str) -> String {
    format!("ssh -p {} dev@{}", port, ssh_domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port_allocation() {
        let mut pool = SshPortPool::new("30000-30002");
        assert_eq!(pool.available(), 3);

        let p1 = pool.allocate("ws-1").unwrap();
        assert_eq!(p1, 30000);
        assert_eq!(pool.available(), 2);

        let p2 = pool.allocate("ws-2").unwrap();
        assert_eq!(p2, 30001);

        // Same slug returns same port
        assert_eq!(pool.allocate("ws-1").unwrap(), 30000);

        let p3 = pool.allocate("ws-3").unwrap();
        assert_eq!(p3, 30002);

        // Pool exhausted
        assert!(pool.allocate("ws-4").is_none());
    }

    #[test]
    fn test_port_deallocation() {
        let mut pool = SshPortPool::new("30000-30001");
        pool.allocate("ws-1");
        pool.allocate("ws-2");
        assert_eq!(pool.available(), 0);

        pool.deallocate("ws-1");
        assert_eq!(pool.available(), 1);

        let p = pool.allocate("ws-3").unwrap();
        assert_eq!(p, 30000); // Reuses freed port
    }

    #[test]
    fn test_ssh_command_format() {
        assert_eq!(
            ssh_command(30042, "ssh.work.ripley.cloud"),
            "ssh -p 30042 dev@ssh.work.ripley.cloud"
        );
    }

    #[test]
    fn test_default_range_on_invalid() {
        let pool = SshPortPool::new("invalid");
        assert_eq!(pool.available(), 2768); // 32767 - 30000 + 1
    }
}
