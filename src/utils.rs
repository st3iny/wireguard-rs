#[cfg(target_os = "linux")]
use std::collections::HashSet;
#[cfg(target_os = "macos")]
use std::io::{BufRead, BufReader, Cursor, Error as IoError};
#[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "netbsd"))]
use std::{io::Write, process::Stdio};
use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    process::Command,
};

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "netbsd"))]
use crate::bsd::add_gateway;
#[cfg(target_os = "linux")]
use crate::{check_command_output_status, netlink, IpVersion};
use crate::{Peer, WireguardInterfaceError};

#[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "netbsd"))]
pub(crate) fn configure_dns(ifname: &str, dns: &[IpAddr]) -> Result<(), WireguardInterfaceError> {
    // Build the resolvconf command
    debug!("Setting up DNS");
    let mut cmd = Command::new("resolvconf");
    let args = ["-a", ifname, "-m", "0", "-x"];
    debug!("Executing command resolvconf with args: {args:?}");
    cmd.args(args);

    // Execute resolvconf command and pipe filtered DNS entries
    if let Ok(mut child) = cmd.stdin(Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            for entry in dns {
                debug!("Adding nameserver entry: {entry}");
                writeln!(stdin, "nameserver {entry}")?;
            }
        }

        let status = child.wait().expect("Failed to wait for command");
        if status.success() {
            return Ok(());
        }
    }

    Err(WireguardInterfaceError::DnsError)
}

#[cfg(target_os = "macos")]
/// Obtain list of network services
fn network_services() -> Result<Vec<String>, IoError> {
    let output = Command::new("networksetup")
        .arg("-listallnetworkservices")
        .output()?;

    if output.status.success() {
        let buf = BufReader::new(Cursor::new(output.stdout));
        // Get all lines from stdout without asterisk (*).
        // An asterisk (*) denotes that a network service is disabled.
        let lines = buf
            .lines()
            .filter_map(|line| line.ok().filter(|line| !line.contains('*')))
            .collect();

        Ok(lines)
    } else {
        Err(IoError::other("command failed"))
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn configure_dns(dns: &[IpAddr]) -> Result<(), WireguardInterfaceError> {
    for service in network_services()? {
        debug!("Setting DNS entries for {service}");
        let mut cmd = Command::new("networksetup");
        cmd.arg("-setdnsservers").arg(&service);
        if dns.is_empty() {
            // This clears all DNS entries.
            cmd.arg("Empty");
        } else {
            cmd.args(dns.iter().map(ToString::to_string));
        }

        if !cmd.status()?.success() {
            warn!("Command `networksetup` failed for {service}");
        }
    }

    Ok(())
}

#[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "netbsd"))]
pub(crate) fn clear_dns(ifname: &str) -> Result<(), WireguardInterfaceError> {
    info!("Removing DNS configuration for interface {ifname}");
    let args = ["-d", ifname, "-f"];
    debug!("Executing resolvconf with args: {args:?}");
    let mut cmd = Command::new("resolvconf");
    let output = cmd.args(args).output()?;
    check_command_output_status(output)?;
    Ok(())
}

#[cfg(target_os = "linux")]
const DEFAULT_FWMARK_TABLE: u32 = 51820;

/// Helper function to add routing.
#[cfg(target_os = "linux")]
pub(crate) fn add_peer_routing(
    peers: &[Peer],
    ifname: &str,
) -> Result<(), WireguardInterfaceError> {
    debug!("Adding peer routing for interface: {ifname}");

    let mut unique_allowed_ips = HashSet::new();
    let mut default_route = None;
    for peer in peers {
        for addr in &peer.allowed_ips {
            if addr.ip.is_unspecified() {
                // Handle default route
                default_route = Some(addr);
                break;
            }
            unique_allowed_ips.insert(addr);
        }
    }

    // If there is default route skip adding other routes.
    if let Some(default_route) = default_route {
        debug!("Found default route: {default_route:?}");
        let is_ipv6 = default_route.ip.is_ipv6();
        let proto = if is_ipv6 { "-6" } else { "-4" };

        let mut host = netlink::get_host(ifname)?;
        debug!("Current host: {host:?}");

        let fwmark = match host.fwmark {
            Some(fwmark) if fwmark != 0 => fwmark,
            Some(_) | None => {
                let mut table = DEFAULT_FWMARK_TABLE;
                loop {
                    let output = Command::new("ip")
                        .args([proto, "route", "show", "table", &table.to_string()])
                        .output()?;
                    if output.stdout.is_empty() {
                        host.fwmark = Some(table);
                        netlink::set_host(ifname, &host)?;
                        debug!("Assigned fwmark: {table}");
                        break;
                    }
                    table += 1;
                }
                table
            }
        };
        debug!("Using fwmark: {fwmark}");
        // Add table rules
        debug!("Adding route for allowed IP: {default_route}");
        netlink::add_route(ifname, default_route, Some(fwmark))?;
        netlink::add_rule(default_route, fwmark)?;

        debug!("Adding rule for main table");
        netlink::add_main_table_rule(default_route, 0)?;

        if is_ipv6 {
            debug!("Reloading ip6tables");
            let output = Command::new("ip6tables-restore").arg("-n").output()?;
            check_command_output_status(output)?;
        } else {
            debug!("Setting systemctl net.ipv4.conf.all.src_valid_mark=1");
            let output = Command::new("sysctl")
                .args(["-q", "net.ipv4.conf.all.src_valid_mark=1"])
                .output()?;
            check_command_output_status(output)?;

            debug!("Reloading iptables");
            let output = Command::new("iptables-restore").arg("-n").output()?;
            check_command_output_status(output)?;
        }
    } else {
        for allowed_ip in unique_allowed_ips {
            debug!("Processing allowed IP: {allowed_ip}");
            netlink::add_route(ifname, allowed_ip, None)?;
        }
    }
    info!("Peers routing added successfully");
    Ok(())
}

/// Helper function to add routing.
#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "netbsd"))]
pub(crate) fn add_peer_routing(
    peers: &[Peer],
    ifname: &str,
) -> Result<(), WireguardInterfaceError> {
    debug!("Adding peer routing for interface: {ifname}");
    for peer in peers {
        for addr in &peer.allowed_ips {
            let mut new_addr = addr.clone();
            // FIXME: currently it is impossible to add another default route, so use the hack from wg-quick for Darwin.
            if addr.ip.is_unspecified() && addr.cidr == 0 {
                new_addr.cidr = 1;
            }
            // Equivalent to `route -qn add -inet[6] <allowed_ip> -interface <ifname>`.
            match add_gateway(&new_addr, ifname) {
                Ok(()) => debug!("Route to {addr} has been added for interface {ifname}"),
                Err(err) => error!("Failed to add route to {addr} for interface {ifname}: {err}"),
            }
        }
    }

    info!("Peers routing added successfully");
    Ok(())
}

/// Clean fwmark rules while removing interface same as in wg-quick
#[cfg(target_os = "linux")]
pub(crate) fn clean_fwmark_rules(fwmark: u32) -> Result<(), WireguardInterfaceError> {
    debug!("Removing firewall rules.");
    netlink::delete_rule(IpVersion::IPv4, fwmark)?;
    netlink::delete_main_table_rule(IpVersion::IPv4, 0)?;
    netlink::delete_rule(IpVersion::IPv6, fwmark)?;
    netlink::delete_main_table_rule(IpVersion::IPv6, 0)?;
    Ok(())
}

/// Resolves domain name to [`SocketAddr`].
pub fn resolve(addr: &str) -> Result<SocketAddr, WireguardInterfaceError> {
    let error = || WireguardInterfaceError::PeerConfigurationError;
    addr.to_socket_addrs()
        .map_err(|_| error())?
        .next()
        .ok_or_else(error)
}
