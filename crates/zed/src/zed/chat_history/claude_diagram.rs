use quick_xml::{
    Reader, Writer,
    events::{BytesEnd, BytesStart, BytesText, Event},
};

// Claude Desktop supplies these widget classes outside the SVG. Use a light
// canvas so text, connectors and node fills stay legible in either editor theme.
const STYLE: &str = "text{font:14px sans-serif;fill:#303438}.th{font-size:14px;font-weight:600}.ts{font-size:12px}.arr{fill:none;stroke:#787872;stroke-width:1.4}.box,.c-gray rect{fill:#f1efe8;stroke:#87857c}.c-purple rect{fill:#eeedfe;stroke:#807bd9}.c-teal rect{fill:#e1f5ee;stroke:#459d82}.c-coral rect,.c-orange rect{fill:#faece7;stroke:#c28067}.c-blue rect{fill:#e5f1fa;stroke:#6594bf}.c-green rect{fill:#eaf3de;stroke:#86a15d}.c-amber rect,.c-yellow rect{fill:#faf0ce;stroke:#aca05e}.c-pink rect,.c-red rect{fill:#fbe9ef;stroke:#be8699}";

pub(super) fn from_tool(name: &str, input: &str) -> Option<(Vec<u8>, f32)> {
    if name != "mcp__visualize__show_widget" || input.len() > 2_000_000 {
        return None;
    }
    let input: serde_json::Value = serde_json::from_str(input).ok()?;
    let svg = input["widget_code"].as_str()?.trim();
    let mut reader = Reader::from_str(svg);
    let mut writer = Writer::new(Vec::new());
    let mut depth = 0usize;
    let mut ratio = None;
    loop {
        let event = reader.read_event().ok()?;
        match event {
            Event::Start(ref node) | Event::Empty(ref node) => {
                let tag = node.name();
                let tag = std::str::from_utf8(tag.as_ref()).ok()?;
                if depth == 0 && (tag != "svg" || ratio.is_some()) {
                    return None;
                }
                if !matches!(
                    tag,
                    "svg"
                        | "g"
                        | "defs"
                        | "marker"
                        | "path"
                        | "line"
                        | "rect"
                        | "circle"
                        | "ellipse"
                        | "polyline"
                        | "polygon"
                        | "text"
                        | "tspan"
                        | "title"
                        | "desc"
                        | "clipPath"
                        | "linearGradient"
                        | "radialGradient"
                        | "stop"
                ) {
                    if matches!(event, Event::Start(_)) {
                        reader.read_to_end(node.name()).ok()?;
                    }
                    continue;
                }
                let mut out = BytesStart::new(tag);
                for attr in node.attributes() {
                    let attr = attr.ok()?;
                    let key = std::str::from_utf8(attr.key.as_ref()).ok()?;
                    if !matches!(
                        key,
                        "viewBox"
                            | "x"
                            | "y"
                            | "width"
                            | "height"
                            | "x1"
                            | "x2"
                            | "y1"
                            | "y2"
                            | "d"
                            | "points"
                            | "cx"
                            | "cy"
                            | "r"
                            | "rx"
                            | "ry"
                            | "dx"
                            | "dy"
                            | "class"
                            | "id"
                            | "fill"
                            | "stroke"
                            | "stroke-width"
                            | "stroke-linecap"
                            | "stroke-linejoin"
                            | "stroke-dasharray"
                            | "transform"
                            | "opacity"
                            | "fill-opacity"
                            | "stroke-opacity"
                            | "fill-rule"
                            | "clip-path"
                            | "marker-end"
                            | "marker-start"
                            | "markerWidth"
                            | "markerHeight"
                            | "orient"
                            | "refX"
                            | "refY"
                            | "text-anchor"
                            | "dominant-baseline"
                            | "font-size"
                            | "font-weight"
                            | "offset"
                            | "stop-color"
                    ) {
                        continue;
                    }
                    let value = std::str::from_utf8(attr.value.as_ref()).ok()?;
                    if depth == 0 {
                        if matches!(key, "width" | "height") {
                            continue;
                        }
                        if key == "viewBox" {
                            let dims: Vec<f32> = value
                                .split(|c: char| c.is_whitespace() || c == ',')
                                .filter(|s| !s.is_empty())
                                .map(str::parse)
                                .collect::<Result<_, _>>()
                                .ok()?;
                            if dims.len() != 4
                                || !dims.iter().all(|n| n.is_finite())
                                || dims[2] <= 0.
                                || dims[3] <= 0.
                            {
                                return None;
                            }
                            let aspect = dims[2] / dims[3];
                            if !(0.02..=50.).contains(&aspect) {
                                return None;
                            }
                            ratio = Some(aspect);
                            let width = 1400f32.min(1800. * aspect);
                            out.push_attribute(("width", width.round().to_string().as_str()));
                            out.push_attribute((
                                "height",
                                (width / aspect).round().to_string().as_str(),
                            ));
                        }
                    }
                    if value.starts_with("var(") || value == "context-stroke" {
                        out.push_attribute((key, "#686866"));
                    } else {
                        out.push_attribute(attr);
                    }
                }
                if depth == 0 {
                    out.push_attribute(("xmlns", "http://www.w3.org/2000/svg"));
                }
                if matches!(event, Event::Empty(_)) {
                    writer.write_event(Event::Empty(out)).ok()?;
                } else {
                    writer.write_event(Event::Start(out)).ok()?;
                    if depth == 0 {
                        writer
                            .write_event(Event::Start(BytesStart::new("style")))
                            .ok()?;
                        writer
                            .write_event(Event::Text(BytesText::new(STYLE)))
                            .ok()?;
                        writer
                            .write_event(Event::End(BytesEnd::new("style")))
                            .ok()?;
                        let mut bg = BytesStart::new("rect");
                        bg.push_attribute(("x", "-100%"));
                        bg.push_attribute(("y", "-100%"));
                        bg.push_attribute(("width", "300%"));
                        bg.push_attribute(("height", "300%"));
                        bg.push_attribute(("fill", "#ffffff"));
                        writer.write_event(Event::Empty(bg)).ok()?;
                    }
                    depth += 1;
                    if depth > 64 {
                        return None;
                    }
                }
            }
            Event::End(tag) => {
                depth = depth.checked_sub(1)?;
                writer.write_event(Event::End(tag)).ok()?;
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) if depth > 0 => {
                writer.write_event(event).ok()?;
            }
            Event::DocType(_) => return None,
            Event::Eof => break,
            _ => {}
        }
    }
    (depth == 0).then_some((writer.into_inner(), ratio?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_widget_renders_colored_nodes_without_loading_external_content() {
        let input = serde_json::json!({"widget_code": r#"<svg width="100%" viewBox="0 0 200 100"><g class="c-teal" onclick="sendPrompt('question')"><rect x="10" y="10" width="100" height="50"/><text x="20" y="30">App DB</text></g><image href="file:///image.png"/></svg>"#});
        let (svg, ratio) = from_tool("mcp__visualize__show_widget", &input.to_string()).unwrap();
        assert_eq!(ratio, 2.);
        let source = std::str::from_utf8(&svg).unwrap();
        assert!(!source.contains("sendPrompt") && !source.contains("file:"));
        let renderer = gpui::SvgRenderer::new(std::sync::Arc::new(()));
        let image = renderer.render_single_frame(&svg, 0.25).unwrap();
        assert!(
            image
                .as_bytes(0)
                .unwrap()
                .chunks_exact(4)
                .any(|pixel| pixel == [0xee, 0xf5, 0xe1, 0xff])
        );
    }
}
