//! Page Layout view through real pointer and keyboard input: cells are hit through the page
//! transform, and headers are edited in place, committed on a click elsewhere, and undoable.

use egui::{Event, Key, Modifiers, PointerButton, Pos2, pos2, vec2};
use gridcraft_engine::Session;
use gridcraft_engine::core::CellRef;
use gridcraft_engine::model::SheetViewMode;
use gridcraft_ui_egui::SheetApp;
use gridcraft_ui_egui::grid::Geo;
use serde_json::json;

type Harness = egui_kittest::Harness<'static, SheetApp>;

fn fixture() -> Harness {
    let mut session = Session::new();
    session.new_workbook();
    for r in 1..=60 {
        session.execute("cell.set", json!({"cell": format!("A{r}"), "input": r.to_string()})).unwrap();
    }
    session.execute("pageLayout.headerFooter", json!({"header": "&LLeft"})).unwrap();
    session.execute("view.pageLayout", json!({})).unwrap();
    let mut h = egui_kittest::Harness::builder()
        .with_size(vec2(1200.0, 900.0))
        .with_step_dt(1.0 / 60.0)
        .with_os(egui::os::OperatingSystem::Mac)
        .build_ui_state(
            |ui, app: &mut SheetApp| {
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                app.ui(ui);
            },
            SheetApp::new(session, Default::default()),
        );
    h.run_steps(4);
    h
}

fn geo(h: &Harness) -> Geo {
    h.state().grid.geo.clone().unwrap()
}

fn click(h: &mut Harness, p: Pos2) {
    // Keep clicks apart from egui's double-click window.
    h.run_steps(40);
    h.input_mut().events.push(Event::PointerMoved(p));
    h.input_mut().events.push(Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
    h.step();
    h.input_mut().events.push(Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
    h.run_steps(3);
}

fn header(h: &Harness) -> String {
    h.state().session.doc().unwrap().wb.active().unwrap().print.header.clone()
}

/// Screen point of a layout point.
fn screen(g: &Geo, x: f64, y: f64) -> Pos2 {
    pos2(g.cells.left() + (x - g.scroll.x as f64) as f32 * g.z, g.cells.top() + (y - g.scroll.y as f64) as f32 * g.z)
}

#[test]
fn cells_are_hit_through_the_page_transform() {
    let mut h = fixture();
    let g = geo(&h);
    let pg = g.paper.clone().expect("Page Layout view lays out pages");
    let app = h.state();
    let sh = app.session.doc().unwrap().wb.active().unwrap();
    assert_eq!(sh.view, SheetViewMode::PageLayout);
    // Cells start inside the first page's margins, not at the grid's corner.
    let a1 = g.cell_rect(sh, CellRef::new(0, 0));
    let content = screen(&g, pg.page_x(0) + pg.margins[0], pg.page_y(0) + pg.margins[2]);
    assert!((a1.left() - content.x).abs() < 0.5 && (a1.top() - content.y).abs() < 0.5, "{a1:?} vs {content:?}");
    let c5 = g.cell_rect(sh, CellRef::parse("C5").unwrap()).center();
    assert_eq!(g.cell_at(sh, c5), CellRef::parse("C5").unwrap());
    click(&mut h, c5);
    assert_eq!(h.state().session.doc().unwrap().selection.active, CellRef::parse("C5").unwrap());
    // A click in the gap left of the page selects nothing new.
    let gap = screen(&g, pg.gap / 2.0, pg.page_y(0) + pg.margins[2] + 30.0);
    click(&mut h, gap);
    assert_eq!(h.state().session.doc().unwrap().selection.active, CellRef::parse("C5").unwrap());
}

#[test]
fn header_is_edited_in_place_committed_by_a_click_elsewhere_and_undoable() {
    let mut h = fixture();
    let g = geo(&h);
    let pg = g.paper.clone().unwrap();
    // The centre third of the first page's top margin.
    let (px, py, w, _) = pg.page_rect(0, 0);
    let centre = screen(&g, px + w / 2.0, py + pg.margins[2] / 2.0);
    click(&mut h, centre);
    {
        let app = h.state();
        let e = app.grid.hf_edit.as_ref().expect("clicking the top margin edits the header");
        assert_eq!((e.footer, e.section), (false, 1));
        assert_eq!(app.ui.ribbon_tab, "Header & Footer");
    }
    h.input_mut().events.push(Event::Text("Report ".into()));
    h.run_steps(2);
    gridcraft_ui_egui::paper_view::insert_field(h.state_mut(), "&[Page]");
    h.run_steps(2);
    assert_eq!(h.state().grid.hf_edit.as_ref().unwrap().texts[1], "Report &[Page]");
    assert_eq!(header(&h), "&LLeft", "nothing is written while editing");
    // Clicking a cell commits (one undo step) and selects the cell.
    let sh_geo = geo(&h);
    let b3 = {
        let app = h.state();
        let sh = app.session.doc().unwrap().wb.active().unwrap();
        sh_geo.cell_rect(sh, CellRef::parse("B3").unwrap()).center()
    };
    click(&mut h, b3);
    assert!(h.state().grid.hf_edit.is_none());
    assert_eq!(header(&h), "&LLeft&CReport &P");
    assert_eq!(h.state().session.doc().unwrap().selection.active, CellRef::parse("B3").unwrap());
    assert_ne!(h.state().ui.ribbon_tab, "Header & Footer");
    h.state_mut().run("edit.undo", json!({})).unwrap();
    assert_eq!(header(&h), "&LLeft");
    // Escape cancels an edit without writing.
    click(&mut h, centre);
    h.input_mut().events.push(Event::Text("x".into()));
    h.run_steps(2);
    h.input_mut().events.push(Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE });
    h.run_steps(2);
    assert!(h.state().grid.hf_edit.is_none());
    assert_eq!(header(&h), "&LLeft");
}

#[test]
fn the_first_page_edits_its_own_header_when_different_first_page_is_on() {
    let mut h = fixture();
    h.state_mut().run("pageLayout.headerFooter", json!({"differentFirst": true, "firstHeader": "&CCover"})).unwrap();
    h.run_steps(3);
    // Insert › Header & Footer edits the first page's centre header, here the first-page one.
    gridcraft_ui_egui::paper_view::start_editing(h.state_mut());
    h.run_steps(3);
    {
        let e = h.state().grid.hf_edit.as_ref().expect("Insert › Header & Footer starts editing");
        assert_eq!((e.footer, e.section, e.kind), (false, 1, gridcraft_engine::model::HfPage::First));
        assert_eq!(e.texts[1], "Cover");
    }
    h.input_mut().events.push(Event::Text(" page".into()));
    h.run_steps(2);
    let g = geo(&h);
    let b3 = {
        let app = h.state();
        g.cell_rect(app.session.doc().unwrap().wb.active().unwrap(), CellRef::parse("B3").unwrap()).center()
    };
    click(&mut h, b3);
    let pr = h.state().session.doc().unwrap().wb.active().unwrap().print.clone();
    assert_eq!(pr.first_header, "&CCover page");
    assert_eq!(pr.header, "&LLeft", "the other pages keep their header");
}

#[test]
fn insert_header_and_footer_switches_to_page_layout_view() {
    let mut h = fixture();
    h.state_mut().run("view.normal", json!({})).unwrap();
    h.run_steps(3);
    gridcraft_ui_egui::paper_view::start_editing(h.state_mut());
    h.run_steps(4);
    assert_eq!(h.state().session.doc().unwrap().wb.active().unwrap().view, SheetViewMode::PageLayout);
    let e = h.state().grid.hf_edit.as_ref().expect("the header is being edited");
    assert_eq!((e.footer, e.section, e.texts[0].as_str()), (false, 1, "Left"));
}
