//! Layout: exact rectangles for nested rows, columns and stacks at several
//! sizes and scales, and the rules every layout keeps.

use std::rc::Rc;

use manguebyte_gfx::{IRect, Rect, Size};
use manguebyte_plugin::editor::EditorContext;
use manguebyte_plugin::editor::testing::RecordingEditorHost;
use manguebyte_ui::layout::{Align, Arrange, Insets, Justify, LayoutNode, Style, UNBOUNDED, solve};
use manguebyte_ui::{IntoView, Root, canvas, column, row, stack};

/// The tree the exact tests use:
///
/// ```text
/// 0 column, padding 10, spacing 10
/// ├── 1 row, spacing 5
/// │   ├── 2 leaf 40 x 30
/// │   ├── 3 leaf 0 x 20, flex 1
/// │   └── 4 leaf 60 x 30
/// ├── 5 row, flex 1
/// │   ├── 6 column, flex 1, padding 4, spacing 4
/// │   │   ├── 7 leaf 20 x 20, aligned to the start
/// │   │   └── 10 leaf, flex 1, at most 50 tall
/// │   └── 8 stack, flex 2
/// │       └── 11 leaf 30 x 30, centred
/// └── 9 leaf 100 x 16, centred
/// ```
fn tree() -> Vec<LayoutNode> {
    let leaf = |w, h| LayoutNode::leaf(Style::leaf(), Size::new(w, h));
    vec![
        LayoutNode::new(
            Style::column()
                .with_padding(Insets::all(10.0))
                .with_spacing(10.0),
            Size::default(),
            vec![1, 5, 9],
        ),
        LayoutNode::new(
            Style::row().with_spacing(5.0),
            Size::default(),
            vec![2, 3, 4],
        ),
        leaf(40.0, 30.0),
        LayoutNode::leaf(Style::leaf().with_flex(1.0), Size::new(0.0, 20.0)),
        leaf(60.0, 30.0),
        LayoutNode::new(Style::row().with_flex(1.0), Size::default(), vec![6, 8]),
        LayoutNode::new(
            Style::column()
                .with_flex(1.0)
                .with_padding(Insets::all(4.0))
                .with_spacing(4.0),
            Size::default(),
            vec![7, 10],
        ),
        LayoutNode::leaf(
            Style::leaf().with_align_self(Align::Start),
            Size::new(20.0, 20.0),
        ),
        LayoutNode::new(Style::stack().with_flex(2.0), Size::default(), vec![11]),
        LayoutNode::leaf(
            Style::leaf().with_align_self(Align::Center),
            Size::new(100.0, 16.0),
        ),
        LayoutNode::leaf(
            Style::leaf()
                .with_flex(1.0)
                .with_max(Size::new(f32::INFINITY, 50.0)),
            Size::default(),
        ),
        LayoutNode::leaf(
            Style::leaf().with_align_self(Align::Center),
            Size::new(30.0, 30.0),
        ),
    ]
}

fn rects(area: Size, scale: f32) -> Vec<Rect> {
    solve(
        &tree(),
        0,
        Rect::new(0.0, 0.0, area.width, area.height),
        scale,
    )
    .into_iter()
    .map(|placement| placement.rect)
    .collect()
}

fn devices(area: Size, scale: f32) -> Vec<IRect> {
    solve(
        &tree(),
        0,
        Rect::new(0.0, 0.0, area.width, area.height),
        scale,
    )
    .into_iter()
    .map(|placement| placement.device)
    .collect()
}

#[test]
fn nested_rows_and_columns_at_300_by_200() {
    let r = rects(Size::new(300.0, 200.0), 1.0);
    assert_eq!(r[0], Rect::new(0.0, 0.0, 300.0, 200.0));
    // The column: 30 for the first row, 16 for the last, two gaps of 10,
    // and the 114 left for the flexible row.
    assert_eq!(r[1], Rect::new(10.0, 10.0, 280.0, 30.0));
    assert_eq!(r[5], Rect::new(10.0, 50.0, 280.0, 114.0));
    assert_eq!(r[9], Rect::new(100.0, 174.0, 100.0, 16.0));
    // The first row: 40 and 60, and the 170 left between them.
    assert_eq!(r[2], Rect::new(10.0, 10.0, 40.0, 30.0));
    assert_eq!(r[3], Rect::new(55.0, 10.0, 170.0, 30.0));
    assert_eq!(r[4], Rect::new(230.0, 10.0, 60.0, 30.0));
    // The second row shares 280 one to two: 93.33 and 186.67, snapped.
    assert_eq!(r[6], Rect::new(10.0, 50.0, 93.0, 114.0));
    assert_eq!(r[8], Rect::new(103.0, 50.0, 187.0, 114.0));
    // Inside the column: 20 at the start; the flexible leaf stops at its
    // maximum of 50, and the rest stays empty.
    assert_eq!(r[7], Rect::new(14.0, 54.0, 20.0, 20.0));
    assert_eq!(r[10], Rect::new(14.0, 78.0, 85.0, 50.0));
    // Centred in the stack: 103.33 + (186.67 - 30) / 2 = 181.67.
    assert_eq!(r[11], Rect::new(182.0, 92.0, 30.0, 30.0));
}

#[test]
fn nested_rows_and_columns_at_400_by_260() {
    let r = rects(Size::new(400.0, 260.0), 1.0);
    assert_eq!(r[1], Rect::new(10.0, 10.0, 380.0, 30.0));
    assert_eq!(r[5], Rect::new(10.0, 50.0, 380.0, 174.0));
    assert_eq!(r[9], Rect::new(150.0, 234.0, 100.0, 16.0));
    assert_eq!(r[3], Rect::new(55.0, 10.0, 270.0, 30.0));
    assert_eq!(r[4], Rect::new(330.0, 10.0, 60.0, 30.0));
    assert_eq!(r[6], Rect::new(10.0, 50.0, 127.0, 174.0));
    assert_eq!(r[8], Rect::new(137.0, 50.0, 253.0, 174.0));
    assert_eq!(r[10], Rect::new(14.0, 78.0, 119.0, 50.0));
    assert_eq!(r[11], Rect::new(248.0, 122.0, 30.0, 30.0));
}

#[test]
fn too_little_room_shrinks_children_towards_their_minimum() {
    let r = rects(Size::new(100.0, 100.0), 1.0);
    // 60 of room for 30 and 16: the flexible row gets the 14 left.
    assert_eq!(r[1], Rect::new(10.0, 10.0, 80.0, 30.0));
    assert_eq!(r[5], Rect::new(10.0, 50.0, 80.0, 14.0));
    // Centred, but no wider than the room.
    assert_eq!(r[9], Rect::new(10.0, 74.0, 80.0, 16.0));
    // 70 of room for 40 and 60: both give up 30 %, the flexible one has
    // nothing.
    assert_eq!(r[2], Rect::new(10.0, 10.0, 28.0, 30.0));
    assert_eq!(r[3], Rect::new(43.0, 10.0, 0.0, 30.0));
    assert_eq!(r[4], Rect::new(48.0, 10.0, 42.0, 30.0));
}

#[test]
fn the_same_tree_at_2x_and_1_5x_snaps_to_device_pixels() {
    let d = devices(Size::new(300.0, 200.0), 2.0);
    assert_eq!(d[0], IRect::new(0, 0, 600, 400));
    assert_eq!(d[3], IRect::new(110, 20, 340, 60));
    // 93.33 and 186.67 become 186.67 and 373.33 device pixels.
    assert_eq!(d[6], IRect::new(20, 100, 187, 228));
    assert_eq!(d[8], IRect::new(207, 100, 373, 228));
    assert_eq!(d[10], IRect::new(28, 156, 171, 100));
    assert_eq!(d[11], IRect::new(363, 184, 60, 60));
    let r = rects(Size::new(300.0, 200.0), 2.0);
    assert_eq!(r[6], Rect::new(10.0, 50.0, 93.5, 114.0));
    assert_eq!(r[11], Rect::new(181.5, 92.0, 30.0, 30.0));

    let d = devices(Size::new(300.0, 200.0), 1.5);
    assert_eq!(d[0], IRect::new(0, 0, 450, 300));
    assert_eq!(d[5], IRect::new(15, 75, 420, 171));
    assert_eq!(d[6], IRect::new(15, 75, 140, 171));
    assert_eq!(d[8], IRect::new(155, 75, 280, 171));
    assert_eq!(d[9], IRect::new(150, 261, 150, 24));
    // 181.67 and 211.67 logical are 272.5 and 317.5 device: half away
    // from zero.
    assert_eq!(d[11], IRect::new(273, 138, 45, 45));
}

#[test]
fn justify_places_what_is_left_over() {
    let mut nodes = vec![
        LayoutNode::new(Style::row().with_spacing(10.0), Size::default(), vec![1, 2]),
        LayoutNode::leaf(Style::leaf(), Size::new(20.0, 10.0)),
        LayoutNode::leaf(Style::leaf(), Size::new(30.0, 10.0)),
    ];
    let mut x = |justify| {
        nodes[0].style = nodes[0].style.with_justify(justify);
        let placed = solve(&nodes, 0, Rect::new(0.0, 0.0, 100.0, 10.0), 1.0);
        (placed[1].rect.x, placed[2].rect.x)
    };
    assert_eq!(x(Justify::Start), (0.0, 30.0));
    assert_eq!(x(Justify::Center), (20.0, 50.0));
    assert_eq!(x(Justify::End), (40.0, 70.0));
    assert_eq!(x(Justify::SpaceBetween), (0.0, 70.0));
}

#[test]
fn minimums_and_maximums_hold_across_and_along() {
    let nodes = vec![
        LayoutNode::new(
            Style::row().with_align(Align::Start),
            Size::default(),
            vec![1, 2, 3],
        ),
        // At least 50 wide, at most 20 tall although stretched.
        LayoutNode::leaf(
            Style::leaf()
                .with_min(Size::new(50.0, 0.0))
                .with_max(Size::new(f32::INFINITY, 20.0))
                .with_align_self(Align::Stretch),
            Size::new(10.0, 10.0),
        ),
        // Flexible, between 30 and 40.
        LayoutNode::leaf(
            Style::leaf()
                .with_flex(1.0)
                .with_min(Size::new(30.0, 0.0))
                .with_max(Size::new(40.0, f32::INFINITY)),
            Size::new(0.0, 5.0),
        ),
        // A maximum below the minimum: the minimum wins.
        LayoutNode::leaf(
            Style::leaf()
                .with_min(Size::new(25.0, 25.0))
                .with_max(Size::new(5.0, 5.0)),
            Size::default(),
        ),
    ];
    let placed = solve(&nodes, 0, Rect::new(0.0, 0.0, 200.0, 100.0), 1.0);
    assert_eq!(placed[1].rect, Rect::new(0.0, 0.0, 50.0, 20.0));
    assert_eq!(placed[2].rect, Rect::new(50.0, 0.0, 40.0, 5.0));
    assert_eq!(placed[3].rect, Rect::new(90.0, 0.0, 25.0, 25.0));
    assert_eq!(UNBOUNDED.width, f32::INFINITY);
}

#[test]
fn the_root_lays_views_out_as_solve_does() {
    // The same shape built from views: canvases have the natural sizes of
    // the leaves above.
    let leaf = |name: &str, w: f32, h: f32| canvas(name, Size::new(w, h), |_, _| {});
    let view = column((
        row((
            leaf("a", 40.0, 30.0),
            leaf("b", 0.0, 20.0).flex(1.0),
            leaf("c", 60.0, 30.0),
        ))
        .spacing(5.0),
        row((
            column((
                leaf("d", 20.0, 20.0).align_self(Align::Start),
                leaf("e", 0.0, 0.0)
                    .flex(1.0)
                    .max_size(Size::new(f32::INFINITY, 50.0)),
            ))
            .flex(1.0)
            .padding(4.0)
            .spacing(4.0),
            stack((leaf("f", 30.0, 30.0).align_self(Align::Center),)).flex(2.0),
        ))
        .flex(1.0),
        leaf("g", 100.0, 16.0).align_self(Align::Center),
    ))
    .padding(10.0)
    .spacing(10.0);
    let (context, _link) = EditorContext::attach(Rc::new(RecordingEditorHost::new()));
    let mut root = Root::new(view, Size::new(300.0, 200.0), 1.5, context);
    root.frame(std::time::Duration::ZERO);
    let expected = devices(Size::new(300.0, 200.0), 1.5);
    let tree = root.access_tree();
    // Tree order: column, row, a, b, c, row, column, d, e, stack, f, g.
    let order = [0, 1, 2, 3, 4, 5, 6, 7, 10, 8, 11, 9];
    assert_eq!(tree.len(), order.len());
    for (entry, &index) in tree.iter().zip(&order) {
        assert_eq!(entry.device, expected[index], "{:?}", entry.node.name);
    }
}

#[test]
fn padding_on_a_leaf_is_part_of_its_natural_size_and_its_content_goes_inside() {
    let nodes = vec![
        LayoutNode::new(
            Style::row().with_align(Align::Start),
            Size::default(),
            vec![1, 2],
        ),
        LayoutNode::leaf(
            Style::leaf().with_padding(Insets::new(1.0, 2.0, 3.0, 4.0)),
            Size::new(10.0, 10.0),
        ),
        LayoutNode::leaf(Style::leaf(), Size::new(5.0, 5.0)),
    ];
    let placed = solve(&nodes, 0, Rect::new(0.0, 0.0, 100.0, 50.0), 1.0);
    // 10 wide and 1 + 3 of padding; 10 tall and 2 + 4.
    assert_eq!(placed[1].rect, Rect::new(0.0, 0.0, 14.0, 16.0));
    assert_eq!(placed[2].rect, Rect::new(14.0, 0.0, 5.0, 5.0));
    let content = placed[1].inset(nodes[1].style.padding, 1.0);
    assert_eq!(content.rect, Rect::new(1.0, 2.0, 10.0, 10.0));
    assert_eq!(content.device, IRect::new(1, 2, 10, 10));
    // At 1.5x the content's edges snap to device pixels too: 1.5, 3, 16.5
    // and 18 become 2, 3, 17 and 18.
    let placed = solve(&nodes, 0, Rect::new(0.0, 0.0, 100.0, 50.0), 1.5);
    let content = placed[1].inset(nodes[1].style.padding, 1.5);
    assert_eq!(content.device, IRect::new(2, 3, 15, 15));
    // Padding larger than the room leaves nothing, inside the placement.
    let squeezed = placed[1].inset(Insets::all(100.0), 1.5);
    assert!(squeezed.rect.is_empty());
    assert!(placed[1].device.contains_rect(&squeezed.device));
}

#[test]
fn padding_on_a_widget_makes_room_around_it() {
    use manguebyte_ui::testing::Harness;
    use manguebyte_ui::{Role, label};

    let height = |view: manguebyte_ui::View| {
        let mut ui = Harness::new(column((view,)), Size::new(200.0, 100.0), 1.0);
        ui.frame();
        ui.get(Role::Label, "Gain").bounds.height
    };
    let plain = height(label("Gain").align_self(Align::Start));
    let padded = height(label("Gain").padding(10.0).align_self(Align::Start));
    assert_eq!(
        padded,
        plain + 20.0,
        "`.padding(10.0)` on a label should keep 10 free above and below"
    );
}

#[test]
fn a_widget_with_padding_draws_inside_it() {
    use manguebyte_gfx::Color;
    use manguebyte_ui::Role;
    use manguebyte_ui::testing::Harness;

    let swatch = canvas("Swatch", Size::new(40.0, 20.0), |cx, scene| {
        scene.fill(cx.bounds(), Color::hex(0x808080));
    })
    .padding(5.0);
    let mut ui = Harness::new(
        row((swatch,)).align(Align::Start),
        Size::new(100.0, 50.0),
        2.0,
    );
    ui.frame();
    let found = ui.get(Role::Image, "Swatch");
    // Its bounds take the padding in; what it draws does not.
    assert_eq!(found.bounds, Rect::new(0.0, 0.0, 50.0, 30.0));
    assert!(
        ui.dump_of(&found).starts_with("clip rect(5 5 40 20)"),
        "{}",
        ui.dump_of(&found)
    );
    let grey =
        |x, y| ui.pixels().pixel(x, y).map(|p| p.to_array()) == Some([0x80, 0x80, 0x80, 0xFF]);
    assert!(!grey(9, 9), "the padding is left free");
    assert!(grey(10, 10) && grey(89, 49), "the content fills the rest");
    assert!(!grey(90, 50));
}

/// A small generator of numbers, the same on every platform.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn length(&mut self, most: u64) -> f32 {
        self.below(most * 4) as f32 / 4.0
    }
}

/// A random tree that fits in a large area.
fn random_tree(random: &mut Random) -> Vec<LayoutNode> {
    let mut nodes = vec![LayoutNode::new(
        Style::column(),
        Size::default(),
        Vec::new(),
    )];
    let mut open = vec![(0usize, 0u32)];
    while let Some((parent, depth)) = open.pop() {
        let count = if depth >= 4 { 0 } else { random.below(5) };
        for _ in 0..count {
            let index = nodes.len();
            let arrange = match random.below(4) {
                0 if depth < 3 => Arrange::Row,
                1 if depth < 3 => Arrange::Column,
                2 if depth < 3 => Arrange::Stack,
                _ => Arrange::Leaf,
            };
            let align =
                [Align::Start, Align::Center, Align::End, Align::Stretch][random.below(4) as usize];
            let mut style = Style {
                arrange,
                padding: Insets::new(
                    random.length(6),
                    random.length(6),
                    random.length(6),
                    random.length(6),
                ),
                spacing: random.length(8),
                align,
                ..Style::leaf()
            };
            // Weights and limits only on leaves, so that every container
            // gets at least its natural size and its children fit; how
            // children shrink when they do not is tested above.
            if arrange == Arrange::Leaf {
                if random.below(3) == 0 {
                    style.flex = random.length(3);
                }
                if random.below(4) == 0 {
                    style.min = Size::new(random.length(20), random.length(20));
                }
                if random.below(4) == 0 {
                    style.max = Size::new(random.length(80), random.length(80));
                }
            }
            if random.below(3) == 0 {
                style.align_self = Some(align);
            }
            nodes.push(LayoutNode::leaf(
                style,
                Size::new(random.length(40), random.length(30)),
            ));
            nodes[parent].children.push(index);
            if arrange != Arrange::Leaf {
                open.push((index, depth + 1));
            }
        }
    }
    nodes
}

#[test]
fn every_layout_keeps_children_inside_in_order_and_on_device_pixels() {
    let mut random = Random(0x5EED_1A70_u64);
    for case in 0..300 {
        let nodes = random_tree(&mut random);
        let scale = [1.0, 1.25, 1.5, 2.0, 3.0][case % 5];
        let placed = solve(&nodes, 0, Rect::new(0.0, 0.0, 4000.0, 3000.0), scale);
        for (index, node) in nodes.iter().enumerate() {
            let parent = placed[index].device;
            // Logical rectangles are the device ones, divided by the scale.
            let rect = placed[index].rect;
            for (logical, device) in [
                (rect.x, parent.x as f32),
                (rect.y, parent.y as f32),
                (rect.width, parent.width as f32),
                (rect.height, parent.height as f32),
            ] {
                assert!((logical * scale - device).abs() < 1e-2, "case {case}");
            }
            let mut previous: Option<IRect> = None;
            for &child in &node.children {
                let device = placed[child].device;
                assert!(
                    parent.contains_rect(&device),
                    "case {case}: child {child} {device:?} outside {index} {parent:?}"
                );
                if let Some(before) = previous {
                    match node.style.arrange {
                        Arrange::Row => {
                            assert!(before.right() <= i64::from(device.x), "case {case}")
                        }
                        Arrange::Column => {
                            assert!(before.bottom() <= i64::from(device.y), "case {case}")
                        }
                        Arrange::Stack | Arrange::Leaf | Arrange::Scroll { .. } => {}
                    }
                }
                previous = Some(device);
            }
        }
    }
}
