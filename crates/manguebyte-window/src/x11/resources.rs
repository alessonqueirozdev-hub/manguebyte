//! The one resource the backend reads: `Xft.dpi`, in which desktops say how
//! large things should be drawn. Plain code, with no system call.

use manguebyte_plugin::editor::checked_scale;

/// The scale the resource database `resources` (the root window's
/// `RESOURCE_MANAGER` property) asks for: its `Xft.dpi` divided by 96.
/// 1 when it names none, or none that gives a usable scale. Of several, the
/// last one counts, as for the X resource manager.
pub(super) fn scale(resources: &[u8]) -> f64 {
    resources
        .split(|&byte| byte == b'\n')
        .filter_map(|line| line.strip_prefix(b"Xft.dpi:"))
        .next_back()
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.trim().parse::<f64>().ok())
        .and_then(|dpi| checked_scale(dpi / 96.0))
        .unwrap_or(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xft_dpi_over_96_is_the_scale() {
        assert_eq!(scale(b"Xft.dpi:\t144\n"), 1.5);
        assert_eq!(
            scale(b"Xcursor.size:\t24\nXft.dpi: 192\nXft.rgba:\trgb\n"),
            2.0
        );
        assert_eq!(scale(b"Xft.dpi:\t96"), 1.0);
        // Desktops write fractions too.
        assert_eq!(scale(b"Xft.dpi:\t120.0\n"), 1.25);
        assert_eq!(scale(b"Xft.dpi:\t96\nXft.dpi:\t48\n"), 0.5);
    }

    #[test]
    fn anything_else_leaves_the_scale_at_one() {
        for resources in [
            &b""[..],
            b"Xft.rgba:\trgb\n",
            b"Xft.dpi:\n",
            b"Xft.dpi:\tlarge\n",
            b"Xft.dpi:\t0\n",
            b"Xft.dpi:\t-96\n",
            b"Xft.dpi:\tNaN\n",
            b"Xft.dpi:\tinf\n",
            b"Xft.dpi:\t100000\n",
            b"Xft.dpi:\t\xff\n",
            // Not this resource.
            b"MyXft.dpi:\t192\n",
            b"Xft.dpix:\t192\n",
        ] {
            assert_eq!(scale(resources), 1.0, "{resources:?}");
        }
    }
}
