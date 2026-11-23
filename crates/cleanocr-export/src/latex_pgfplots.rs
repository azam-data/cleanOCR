use std::fmt::Write as _;

use cleanocr_ir::{AxisValue, ChartIr, ChartKind};

use crate::latex_math::escape_latex;

/// Returns `true` if the chart can be deterministically rendered with `pgfplots`.
pub fn is_supported_pgfplots_chart(chart: &ChartIr) -> bool {
    chart.kind != ChartKind::UnsupportedFallback && !chart.series.is_empty()
}

/// Deterministically compiles a structured `ChartIr` into a `tikzpicture` + `pgfplots` `axis` block.
/// Returns `None` if the chart is unsupported or contains no series, signaling the caller to
/// fall back to `\includegraphics`.
pub fn emit_pgfplots(chart: &ChartIr) -> Option<String> {
    if !is_supported_pgfplots_chart(chart) {
        return None;
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "% CLEANOCR NOTICE: Chart values extracted via ML; verify accuracy."
    );
    let _ = writeln!(out, r"\begin{{tikzpicture}}");
    let _ = writeln!(out, r"\begin{{axis}}[");

    let mut opts: Vec<String> = vec!["width=0.90\\linewidth".to_string()];

    match chart.kind {
        ChartKind::Bar => opts.push("ybar".to_string()),
        ChartKind::HorizontalBar => opts.push("xbar".to_string()),
        ChartKind::StackedBar => opts.push("ybar stacked".to_string()),
        ChartKind::Line | ChartKind::Scatter | ChartKind::UnsupportedFallback => {}
    }

    if let Some(title) = &chart.title {
        opts.push(format!("title={{{}}}", escape_latex(title)));
    }
    if let Some(xlabel) = &chart.x_axis.label {
        opts.push(format!("xlabel={{{}}}", escape_latex(xlabel)));
    }
    if let Some(ylabel) = &chart.y_axis.label {
        opts.push(format!("ylabel={{{}}}", escape_latex(ylabel)));
    }

    if chart.x_axis.is_log_scale {
        opts.push("xmode=log".to_string());
    }
    if chart.y_axis.is_log_scale {
        opts.push("ymode=log".to_string());
    }

    if let Some(min) = chart.x_axis.min {
        opts.push(format!("xmin={}", format_num(min)));
    }
    if let Some(max) = chart.x_axis.max {
        opts.push(format!("xmax={}", format_num(max)));
    }
    if let Some(min) = chart.y_axis.min {
        opts.push(format!("ymin={}", format_num(min)));
    }
    if let Some(max) = chart.y_axis.max {
        opts.push(format!("ymax={}", format_num(max)));
    }

    let is_horizontal = chart.kind == ChartKind::HorizontalBar;
    let categories = if !chart.x_axis.categories.is_empty() {
        &chart.x_axis.categories
    } else {
        &chart.y_axis.categories
    };

    if !categories.is_empty() {
        let joined: Vec<String> = categories
            .iter()
            .map(|c| format_symbolic_coord(c))
            .collect();
        if is_horizontal {
            opts.push(format!("symbolic y coords={{{}}}", joined.join(",")));
            opts.push("ytick=data".to_string());
        } else {
            opts.push(format!("symbolic x coords={{{}}}", joined.join(",")));
            opts.push("xtick=data".to_string());
        }
    }

    if chart.grid {
        opts.push("grid=major".to_string());
    }
    if chart.has_legend {
        opts.push("legend pos=outer north east".to_string());
    }

    for (idx, opt) in opts.iter().enumerate() {
        let suffix = if idx + 1 < opts.len() { "," } else { "" };
        let _ = writeln!(out, "  {opt}{suffix}");
    }
    let _ = writeln!(out, "]");

    for series in &chart.series {
        let mut plot_opts: Vec<String> = Vec::new();
        if chart.kind == ChartKind::Scatter {
            plot_opts.push("only marks".to_string());
        }
        if let Some(hex) = &series.color_hex
            && let Some((r, g, b)) = parse_hex_rgb(hex)
        {
            plot_opts.push(format!("color={{rgb,255:red,{r};green,{g};blue,{b}}}"));
        }

        let coords: Vec<String> = series
            .points
            .iter()
            .map(|pt| {
                let x_str = match &pt.x {
                    AxisValue::Numeric(v) => format_num(*v),
                    AxisValue::Category(cat) => format_symbolic_coord(cat),
                };
                let y_str = format_num(pt.y);
                if is_horizontal {
                    format!("({y_str},{x_str})")
                } else {
                    format!("({x_str},{y_str})")
                }
            })
            .collect();

        if plot_opts.is_empty() {
            let _ = writeln!(out, "  \\addplot coordinates {{ {} }};", coords.join(" "));
        } else {
            let _ = writeln!(
                out,
                "  \\addplot[{}] coordinates {{ {} }};",
                plot_opts.join(", "),
                coords.join(" ")
            );
        }

        if chart.has_legend
            && let Some(name) = &series.name
        {
            let _ = writeln!(out, "  \\addlegendentry{{{}}}", escape_latex(name));
        }
    }

    let _ = writeln!(out, r"\end{{axis}}");
    let _ = writeln!(out, r"\end{{tikzpicture}}");
    Some(out)
}

fn format_symbolic_coord(raw: &str) -> String {
    let escaped = escape_latex(raw.trim());
    if escaped.contains(',') || escaped.contains('=') || escaped.contains(']') {
        format!("{{{escaped}}}")
    } else {
        escaped
    }
}

fn format_num(v: f64) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    let rounded = (v * 1_000_000.0).round() / 1_000_000.0;
    if (rounded - rounded.trunc()).abs() < 1e-9 {
        format!("{}", rounded as i64)
    } else {
        format!("{rounded}")
    }
}

fn parse_hex_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let s = hex.trim().strip_prefix('#').unwrap_or(hex.trim());
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cleanocr_ir::{ChartAxis, ChartPoint, ChartSeries};

    #[test]
    fn emits_categorical_bar_chart_with_symbolic_x_coords() {
        let chart = ChartIr {
            kind: ChartKind::Bar,
            title: Some("Q1-Q2 Revenue (%)".to_string()),
            x_axis: ChartAxis {
                label: Some("Quarter".to_string()),
                is_log_scale: false,
                min: None,
                max: None,
                categories: vec!["Q1, 2026".to_string(), "Q2".to_string()],
            },
            y_axis: ChartAxis {
                label: Some("USD (M)".to_string()),
                is_log_scale: false,
                min: Some(0.0),
                max: Some(100.0),
                categories: Vec::new(),
            },
            series: vec![ChartSeries {
                name: Some("Actual".to_string()),
                color_hex: Some("#1f77b4".to_string()),
                points: vec![
                    ChartPoint {
                        x: AxisValue::Category("Q1, 2026".to_string()),
                        y: 42.5,
                    },
                    ChartPoint {
                        x: AxisValue::Category("Q2".to_string()),
                        y: 58.0,
                    },
                ],
            }],
            has_legend: true,
            grid: true,
            extraction_confidence: 0.91,
        };

        let tex = emit_pgfplots(&chart).expect("should emit pgfplots");
        assert!(tex.contains("ybar"));
        assert!(tex.contains("symbolic x coords={{Q1, 2026},Q2}"));
        assert!(tex.contains("xtick=data"));
        assert!(tex.contains(r"title={Q1-Q2 Revenue (\%)}"));
        assert!(tex.contains("({Q1, 2026},42.5) (Q2,58)"));
        assert!(tex.contains(r"\addlegendentry{Actual}"));
        assert!(tex.contains("color={rgb,255:red,31;green,119;blue,180}"));
    }

    #[test]
    fn emits_log_scale_scatter_plot() {
        let chart = ChartIr {
            kind: ChartKind::Scatter,
            title: None,
            x_axis: ChartAxis {
                label: Some("Frequency".to_string()),
                is_log_scale: true,
                min: Some(1.0),
                max: Some(1000.0),
                categories: Vec::new(),
            },
            y_axis: ChartAxis {
                label: Some("Gain".to_string()),
                is_log_scale: false,
                min: None,
                max: None,
                categories: Vec::new(),
            },
            series: vec![ChartSeries {
                name: None,
                color_hex: None,
                points: vec![
                    ChartPoint {
                        x: AxisValue::Numeric(10.0),
                        y: 1.5,
                    },
                    ChartPoint {
                        x: AxisValue::Numeric(100.0),
                        y: 3.25,
                    },
                ],
            }],
            has_legend: false,
            grid: false,
            extraction_confidence: 0.96,
        };

        let tex = emit_pgfplots(&chart).expect("should emit scatter plot");
        assert!(tex.contains("xmode=log"));
        assert!(tex.contains(r"\addplot[only marks] coordinates { (10,1.5) (100,3.25) };"));
    }

    #[test]
    fn unsupported_or_empty_charts_return_none() {
        let unsupported = ChartIr {
            kind: ChartKind::UnsupportedFallback,
            title: None,
            x_axis: ChartAxis {
                label: None,
                is_log_scale: false,
                min: None,
                max: None,
                categories: Vec::new(),
            },
            y_axis: ChartAxis {
                label: None,
                is_log_scale: false,
                min: None,
                max: None,
                categories: Vec::new(),
            },
            series: vec![ChartSeries {
                name: None,
                color_hex: None,
                points: Vec::new(),
            }],
            has_legend: false,
            grid: false,
            extraction_confidence: 0.5,
        };
        assert!(emit_pgfplots(&unsupported).is_none());
    }
}
