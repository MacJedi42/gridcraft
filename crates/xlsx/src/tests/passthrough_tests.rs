//! Pass-through of unmodelled package content (custom XML, external links, connections, query
//! tables, printer settings, form controls, OLE objects). Packages are built in code.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

use gridcraft_model::Sheet;

use super::make_zip;
use crate::package::{normalize, resolve, split_dir};
use crate::xml;
use crate::{read_xlsx, write_xlsx};

const NS_R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const NS_MAIN: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const PRINTER: &[u8] = &[0, 1, 2, 3, 250, 251, 252, 253, 9, 9, 9];
const OLE: &[u8] = b"\xD0\xCF\x11\xE0 not really a compound file";
const EMF: &[u8] = b"\x01\x00\x00\x00 preview";
const CUSTOM_XML: &str = r#"<?xml version="1.0"?><root xmlns="urn:example:data"><v>42</v></root>"#;
const CUSTOM_PROPS: &str = r#"<?xml version="1.0"?><Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/custom-properties"/>"#;

fn rels(items: &[(&str, &str, &str)]) -> String {
    let mut s = String::from(r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#);
    for (id, ty, target) in items {
        let ty = if ty.starts_with("http") { ty.to_string() } else { format!("{NS_R}/{ty}") };
        let ext = if target.contains("://") { r#" TargetMode="External""# } else { "" };
        s.push_str(&format!(r#"<Relationship Id="{id}" Type="{ty}" Target="{target}"{ext}/>"#));
    }
    s.push_str("</Relationships>");
    s
}

const CONTENT_TYPES: &str = r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="bin" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.printerSettings"/><Default Extension="emf" ContentType="image/x-emf"/><Default Extension="vml" ContentType="application/vnd.openxmlformats-officedocument.vmlDrawing"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/embeddings/oleObject1.bin" ContentType="application/vnd.openxmlformats-officedocument.oleObject"/><Override PartName="/customXml/itemProps1.xml" ContentType="application/vnd.openxmlformats-officedocument.customXmlProperties+xml"/><Override PartName="/xl/externalLinks/externalLink1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.externalLink+xml"/><Override PartName="/xl/connections.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.connections+xml"/><Override PartName="/xl/queryTables/queryTable1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.queryTable+xml"/><Override PartName="/xl/ctrlProps/ctrlProp1.xml" ContentType="application/vnd.ms-excel.controlproperties+xml"/><Override PartName="/docProps/custom.xml" ContentType="application/vnd.openxmlformats-officedocument.custom-properties+xml"/><Override PartName="/xl/comments1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.comments+xml"/></Types>"#;

const EXTERNAL_LINK: &str = r#"<?xml version="1.0"?><externalLink xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><externalBook r:id="rId1"><sheetNames><sheetName val="Sheet1"/></sheetNames><sheetDataSet><sheetData sheetId="0"><row r="1"><cell r="A1"><v>7</v></cell></row></sheetData></sheetDataSet></externalBook></externalLink>"#;
const CONNECTIONS: &str = r#"<?xml version="1.0"?><connections xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><connection id="1" name="Query" type="5" refreshedVersion="6"><dbPr connection="Provider=Example" command="SELECT 1"/></connection></connections>"#;
const QUERY_TABLE: &str = r#"<?xml version="1.0"?><queryTable xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" name="ExternalData_1" connectionId="1" autoFormatId="16" applyNumberFormats="0" applyBorderFormats="0" applyFontFormats="0" applyPatternFormats="0" applyAlignmentFormats="0" applyWidthHeightFormats="0"/>"#;
const CTRL_PROP: &str = r#"<?xml version="1.0"?><formControlPr xmlns="http://schemas.microsoft.com/office/spreadsheetml/2009/9/main" objectType="CheckBox" fmlaLink="$C$1" lockText="1"/>"#;
const COMMENTS: &str = r#"<?xml version="1.0"?><comments xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><authors><author>A</author></authors><commentList><comment ref="A1" authorId="0"><text><t>note</t></text></comment></commentList></comments>"#;
const VML: &str = r##"<xml xmlns:v="urn:schemas-microsoft-com:vml" xmlns:o="urn:schemas-microsoft-com:office:office" xmlns:x="urn:schemas-microsoft-com:office:excel"><o:shapelayout v:ext="edit"><o:idmap v:ext="edit" data="1"/></o:shapelayout><v:shapetype id="_x0000_t75" coordsize="21600,21600" o:spt="75"/><v:shape id="_x0000_s1025" type="#_x0000_t75" style="position:absolute"><v:imagedata o:relid="rId1" o:title=""/><x:ClientData ObjectType="Pict"><x:Anchor>1, 0, 1, 0, 3, 0, 5, 0</x:Anchor><x:CF>Pict</x:CF></x:ClientData></v:shape><v:shape id="_x0000_s1026" type="#_x0000_t201" style="position:absolute"><x:ClientData ObjectType="Checkbox"><x:Anchor>4, 0, 1, 0, 6, 0, 2, 0</x:Anchor><x:FmlaLink>$C$1</x:FmlaLink></x:ClientData></v:shape><v:shape id="_x0000_s1027" type="#_x0000_t202" style="position:absolute"><v:textbox><div>old note<br></div></v:textbox><x:ClientData ObjectType="Note"><x:Row>0</x:Row><x:Column>0</x:Column></x:ClientData></v:shape></xml>"##;

/// A workbook with every kind of kept content: package root custom properties, custom XML, an
/// external link (with formulas using it), a connection, and on sheet "Data" a query table,
/// printer settings, an OLE object (in `mc:AlternateContent`), a form control, a note and the VML
/// drawing all of them share.
fn rich_package() -> Vec<u8> {
    rich_package_with(CONTENT_TYPES, false)
}

/// [`rich_package`] with other content types; `twin` makes sheet "Other" a copy of "Data" that
/// shares its VML drawing and embedded parts.
fn rich_package_with(content_types: &str, twin: bool) -> Vec<u8> {
    let workbook = format!(
        r#"<?xml version="1.0"?><workbook xmlns="{NS_MAIN}" xmlns:r="{NS_R}"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/><sheet name="Other" sheetId="2" r:id="rId2"/></sheets><externalReferences><externalReference r:id="rId3"/></externalReferences><definedNames><definedName name="ExternalData_1" localSheetId="0" hidden="1">Data!$E$1:$E$2</definedName></definedNames></workbook>"#
    );
    let sheet1 = format!(
        r#"<?xml version="1.0"?><worksheet xmlns="{NS_MAIN}" xmlns:r="{NS_R}" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:x14="http://schemas.microsoft.com/office/spreadsheetml/2009/9/main" xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" mc:Ignorable="x14"><sheetData><row r="1"><c r="A1"><f>[1]Sheet1!$A$1</f><v>7</v></c><c r="B1"><f>SUM([1]Sheet1!A1:A3)*2</f><v>14</v></c></row></sheetData><pageMargins left="0.7" right="0.7" top="0.75" bottom="0.75" header="0.3" footer="0.3"/><pageSetup orientation="landscape" r:id="rId1"/><legacyDrawing r:id="rId2"/><mc:AlternateContent><mc:Choice Requires="x14"><oleObjects><mc:AlternateContent><mc:Choice Requires="x14"><oleObject progId="Package" shapeId="1025" r:id="rId3"><objectPr defaultSize="0" r:id="rId4"><anchor moveWithCells="1"><from><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>1</xdr:row><xdr:rowOff>0</xdr:rowOff></from><to><xdr:col>3</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>5</xdr:row><xdr:rowOff>0</xdr:rowOff></to></anchor></objectPr></oleObject></mc:Choice><mc:Fallback><oleObject progId="Package" shapeId="1025" r:id="rId3"/></mc:Fallback></mc:AlternateContent></oleObjects></mc:Choice></mc:AlternateContent><controls><control shapeId="1026" r:id="rId5" name="Check Box 1"/></controls></worksheet>"#
    );
    let sheet2 = if twin {
        sheet1.clone()
    } else {
        format!(r#"<?xml version="1.0"?><worksheet xmlns="{NS_MAIN}"><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#)
    };
    let root_rels = rels(&[("rId1", "officeDocument", "xl/workbook.xml"), ("rId2", "custom-properties", "docProps/custom.xml")]);
    let wb_rels = rels(&[
        ("rId1", "worksheet", "worksheets/sheet1.xml"),
        ("rId2", "worksheet", "worksheets/sheet2.xml"),
        ("rId3", "externalLink", "externalLinks/externalLink1.xml"),
        ("rId4", "customXml", "../customXml/item1.xml"),
        ("rId5", "connections", "connections.xml"),
    ]);
    let sheet1_rels = rels(&[
        ("rId1", "printerSettings", "../printerSettings/printerSettings1.bin"),
        ("rId2", "vmlDrawing", "../drawings/vmlDrawing1.vml"),
        ("rId3", "oleObject", "../embeddings/oleObject1.bin"),
        ("rId4", "image", "../media/image1.emf"),
        ("rId5", "http://schemas.openxmlformats.org/officeDocument/2006/relationships/ctrlProp", "../ctrlProps/ctrlProp1.xml"),
        ("rId6", "queryTable", "../queryTables/queryTable1.xml"),
        ("rId7", "comments", "../comments1.xml"),
    ]);
    let custom_rels = rels(&[("rId1", "customXmlProps", "itemProps1.xml")]);
    let link_rels = rels(&[("rId1", "externalLinkPath", "file:///C:/never/opened/Book2.xlsx")]);
    let vml_rels = rels(&[("rId1", "image", "../media/image1.emf")]);
    let item_props = r#"<?xml version="1.0"?><ds:datastoreItem ds:itemID="{11111111-2222-3333-4444-555555555555}" xmlns:ds="http://schemas.openxmlformats.org/officeDocument/2006/customXml"/>"#;
    let mut files: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", content_types.as_bytes()),
        ("_rels/.rels", root_rels.as_bytes()),
        ("docProps/custom.xml", CUSTOM_PROPS.as_bytes()),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", wb_rels.as_bytes()),
        ("xl/worksheets/sheet1.xml", sheet1.as_bytes()),
        ("xl/worksheets/sheet2.xml", sheet2.as_bytes()),
        ("xl/worksheets/_rels/sheet1.xml.rels", sheet1_rels.as_bytes()),
        ("xl/printerSettings/printerSettings1.bin", PRINTER),
        ("xl/drawings/vmlDrawing1.vml", VML.as_bytes()),
        ("xl/drawings/_rels/vmlDrawing1.vml.rels", vml_rels.as_bytes()),
        ("xl/embeddings/oleObject1.bin", OLE),
        ("xl/media/image1.emf", EMF),
        ("xl/ctrlProps/ctrlProp1.xml", CTRL_PROP.as_bytes()),
        ("xl/queryTables/queryTable1.xml", QUERY_TABLE.as_bytes()),
        ("xl/comments1.xml", COMMENTS.as_bytes()),
        ("customXml/item1.xml", CUSTOM_XML.as_bytes()),
        ("customXml/_rels/item1.xml.rels", custom_rels.as_bytes()),
        ("customXml/itemProps1.xml", item_props.as_bytes()),
        ("xl/externalLinks/externalLink1.xml", EXTERNAL_LINK.as_bytes()),
        ("xl/externalLinks/_rels/externalLink1.xml.rels", link_rels.as_bytes()),
        ("xl/connections.xml", CONNECTIONS.as_bytes()),
    ];
    if twin {
        files.push(("xl/worksheets/_rels/sheet2.xml.rels", sheet1_rels.as_bytes()));
    }
    make_zip(&files)
}

/// An unpacked package: parts by name, content types, resolved relationships.
struct Unpacked {
    parts: HashMap<String, Vec<u8>>,
}

/// (id, type, resolved target, external)
type RelList = Vec<(String, String, String, bool)>;

impl Unpacked {
    fn new(bytes: &[u8]) -> Unpacked {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut parts = HashMap::new();
        for i in 0..z.len() {
            let mut f = z.by_index(i).unwrap();
            let mut v = vec![];
            f.read_to_end(&mut v).unwrap();
            parts.insert(f.name().to_string(), v);
        }
        Unpacked { parts }
    }
    fn text(&self, name: &str) -> String {
        String::from_utf8(self.parts.get(name).unwrap_or_else(|| panic!("no part {name}")).clone()).unwrap()
    }
    fn rels(&self, part: &str) -> RelList {
        let (dir, file) = split_dir(part);
        let name = if dir.is_empty() { format!("_rels/{file}.rels") } else { format!("{dir}/_rels/{file}.rels") };
        let Some(b) = self.parts.get(&name) else { return vec![] };
        let root = xml::parse(b).unwrap();
        root.kids("Relationship")
            .map(|r| {
                let ext = r.attr("TargetMode") == Some("External");
                let t = r.attr("Target").unwrap();
                (r.attr("Id").unwrap().into(), r.attr("Type").unwrap().into(), if ext { t.into() } else { resolve(dir, t) }, ext)
            })
            .collect()
    }
    fn target(&self, part: &str, id: &str) -> (String, String) {
        let r = self.rels(part).into_iter().find(|r| r.0 == id).unwrap_or_else(|| panic!("{part}: no relationship {id}"));
        (r.1, r.2)
    }
    /// Every part has a content type, every internal relationship target exists, rel ids are
    /// unique per part.
    fn assert_valid(&self) {
        let types = xml::parse(self.parts.get("[Content_Types].xml").unwrap()).unwrap();
        let defaults: Vec<String> = types.kids("Default").map(|d| d.attr("Extension").unwrap().to_ascii_lowercase()).collect();
        let overrides: Vec<String> = types.kids("Override").map(|d| normalize(d.attr("PartName").unwrap())).collect();
        let mut seen_override = std::collections::HashSet::new();
        for o in &overrides {
            assert!(seen_override.insert(o.clone()), "duplicate override {o}");
            assert!(self.parts.keys().any(|p| normalize(p) == *o), "override for a missing part {o}");
        }
        for name in self.parts.keys().filter(|n| *n != "[Content_Types].xml") {
            let ext = name.rsplit('.').next().unwrap().to_ascii_lowercase();
            assert!(overrides.contains(&normalize(name)) || defaults.contains(&ext), "{name} has no content type");
            if name.ends_with(".rels") {
                let source = name.replace("_rels/", "").trim_end_matches(".rels").to_string();
                let rels = self.rels(&source);
                let mut ids = std::collections::HashSet::new();
                for (id, _, target, ext) in rels {
                    assert!(ids.insert(id.clone()), "{name}: duplicate id {id}");
                    if !ext {
                        assert!(self.parts.keys().any(|p| normalize(p) == normalize(&target)), "{name}: {id} → missing {target}");
                    }
                }
            }
        }
    }
    /// The part holding exactly `data`.
    fn find(&self, data: &[u8]) -> Vec<&String> {
        self.parts.iter().filter(|(_, v)| v.as_slice() == data).map(|(k, _)| k).collect()
    }
}

/// The value of attribute `attr` of the first `<tag …>` in `x` (`r:id` of `<pageSetup`).
fn attr_of(x: &str, tag: &str, attr: &str) -> String {
    let i = x.find(&format!("<{tag} ")).unwrap_or_else(|| panic!("no <{tag}> in {x}"));
    let head = &x[i..i + x[i..].find('>').unwrap()];
    let j = head.find(&format!(" {attr}=\"")).unwrap_or_else(|| panic!("no {attr} in {head}")) + attr.len() + 3;
    head[j..j + head[j..].find('"').unwrap()].to_string()
}

fn sheet_part(u: &Unpacked, sheet: &str) -> String {
    let wb = u.text("xl/workbook.xml");
    let i = wb.find(&format!("<sheet name=\"{sheet}\"")).unwrap();
    let id = attr_of(&wb[i..], "sheet", "r:id");
    u.target("xl/workbook.xml", &id).1
}

#[test]
fn unmodelled_parts_survive_a_round_trip() {
    let (wb, rep) = read_xlsx(&rich_package()).unwrap();
    assert!(wb.preserved.is_some(), "{:?}", rep.warnings);
    let data = wb.sheet(0).unwrap();
    assert!(data.preserved.is_some(), "{:?}", rep.warnings);
    assert!(!rep.warnings.iter().any(|w| w.contains("dropped")), "{:?}", rep.warnings);
    assert_eq!(data.comments.len(), 1, "the note is modelled");

    let out = write_xlsx(&wb).unwrap();
    let u = Unpacked::new(&out);
    u.assert_valid();
    // Every kept part is in the output byte for byte.
    for d in [
        PRINTER,
        OLE,
        EMF,
        CUSTOM_XML.as_bytes(),
        CUSTOM_PROPS.as_bytes(),
        EXTERNAL_LINK.as_bytes(),
        CONNECTIONS.as_bytes(),
        QUERY_TABLE.as_bytes(),
        CTRL_PROP.as_bytes(),
    ] {
        assert_eq!(u.find(d).len(), 1, "kept part missing or duplicated: {}", String::from_utf8_lossy(d));
    }
    // Package root: custom properties.
    assert!(u.rels("").iter().any(|r| r.1.ends_with("/custom-properties") && u.parts.get(&r.2).is_some_and(|b| b == CUSTOM_PROPS.as_bytes())));
    // Workbook: external link in <externalReferences>, its path kept as an external target.
    let wbx = u.text("xl/workbook.xml");
    let link_id = attr_of(&wbx, "externalReference", "r:id");
    let (ty, link) = u.target("xl/workbook.xml", &link_id);
    assert!(ty.ends_with("/externalLink"));
    assert_eq!(u.parts.get(&link).unwrap(), EXTERNAL_LINK.as_bytes());
    let path = u.rels(&link);
    assert_eq!(path.len(), 1);
    assert_eq!((path[0].0.as_str(), path[0].2.as_str(), path[0].3), ("rId1", "file:///C:/never/opened/Book2.xlsx", true));
    assert!(wbx.find("</sheets>").unwrap() < wbx.find("<externalReferences").unwrap());
    assert!(wbx.find("<externalReferences").unwrap() < wbx.find("<definedNames").unwrap());
    // The custom XML item keeps its properties part.
    let item = u.find(CUSTOM_XML.as_bytes())[0].clone();
    assert!(u.rels(&item).iter().any(|r| r.1.ends_with("/customXmlProps")));
    // Formulas using the link are written as they were.
    let sheet = sheet_part(&u, "Data");
    let sx = u.text(&sheet);
    assert!(sx.contains("<f>[1]Sheet1!$A$1</f>"), "{sx}");
    assert!(sx.contains("<f>SUM([1]Sheet1!A1:A3)*2</f>"), "{sx}");
    // Printer settings (#324).
    let ps = attr_of(&sx, "pageSetup", "r:id");
    let (ty, target) = u.target(&sheet, &ps);
    assert!(ty.ends_with("/printerSettings"));
    assert_eq!(u.parts.get(&target).unwrap(), PRINTER);
    // The OLE object and its preview image, re-linked.
    let ole = attr_of(&sx, "oleObject", "r:id");
    assert_eq!(u.parts.get(&u.target(&sheet, &ole).1).unwrap(), OLE);
    let preview = attr_of(&sx, "objectPr", "r:id");
    assert_eq!(u.parts.get(&u.target(&sheet, &preview).1).unwrap(), EMF);
    // Namespace declarations travel with the fragment.
    assert_eq!(attr_of(&sx, "mc:AlternateContent", "xmlns:mc"), "http://schemas.openxmlformats.org/markup-compatibility/2006");
    assert_eq!(attr_of(&sx, "mc:AlternateContent", "xmlns:x14"), "http://schemas.microsoft.com/office/spreadsheetml/2009/9/main");
    // The form control.
    let ctrl = attr_of(&sx, "control", "r:id");
    assert_eq!(u.parts.get(&u.target(&sheet, &ctrl).1).unwrap(), CTRL_PROP.as_bytes());
    // Schema order: pageSetup < legacyDrawing < oleObjects < controls.
    let pos = |t: &str| sx.find(t).unwrap_or_else(|| panic!("no {t}"));
    assert!(pos("<pageSetup") < pos("<legacyDrawing") && pos("<legacyDrawing") < pos("<oleObjects") && pos("<oleObjects") < pos("<controls"));
    // Query table: a sheet relationship only.
    assert!(u.rels(&sheet).iter().any(|r| r.1.ends_with("/queryTable") && u.parts.get(&r.2).is_some_and(|b| b == QUERY_TABLE.as_bytes())));
    // One VML drawing: the kept control shapes plus GridCraft's note, in a fresh shape block.
    let vml_id = attr_of(&sx, "legacyDrawing", "r:id");
    let vml_part = u.target(&sheet, &vml_id).1;
    let vml = u.text(&vml_part);
    assert!(vml.contains("_x0000_s1025") && vml.contains("_x0000_s1026"));
    assert!(!vml.contains("old note") && !vml.contains("_x0000_s1027"), "the source's note shape is replaced");
    assert_eq!(vml.matches("ObjectType=\"Note\"").count(), 1);
    assert!(vml.contains("_x0000_s2049") && vml.contains("data=\"1,2\""), "{vml}");
    let img = u.rels(&vml_part);
    assert_eq!(img.len(), 1);
    assert_eq!(u.parts.get(&img[0].2).unwrap(), EMF);

    // Reading the output keeps the same content again.
    let (wb2, rep2) = read_xlsx(&out).unwrap();
    assert!(!rep2.warnings.iter().any(|w| w.contains("dropped")), "{:?}", rep2.warnings);
    let a = wb.sheet(0).unwrap().preserved.as_deref().unwrap();
    let b = wb2.sheet(0).unwrap().preserved.as_deref().unwrap();
    let mut da: Vec<&[u8]> = a.parts.iter().map(|p| &*p.data).collect();
    let mut db: Vec<&[u8]> = b.parts.iter().map(|p| &*p.data).collect();
    da.sort();
    db.sort();
    assert_eq!(da, db);
    assert_eq!(a.elements.len(), b.elements.len());
    assert_eq!(wb.preserved.as_deref().unwrap().parts.len(), wb2.preserved.as_deref().unwrap().parts.len());
    u.assert_valid();
}

#[test]
fn edited_workbooks_keep_consistent_relationships() {
    let (mut wb, _) = read_xlsx(&rich_package()).unwrap();
    // Insert a sheet before, rename the sheet with controls, drop its note, edit a cell.
    wb.sheets.insert(0, Arc::new(Sheet::new("New")));
    let s = wb.sheet_mut(1).unwrap();
    s.name = "Renamed".into();
    s.comments.clear();
    s.set_value(gridcraft_core::CellRef::new(5, 5), gridcraft_core::Value::Number(3.0));
    let u = Unpacked::new(&write_xlsx(&wb).unwrap());
    u.assert_valid();
    let sheet = sheet_part(&u, "Renamed");
    assert_eq!(sheet, "xl/worksheets/sheet2.xml");
    let sx = u.text(&sheet);
    assert_eq!(u.parts.get(&u.target(&sheet, &attr_of(&sx, "pageSetup", "r:id")).1).unwrap(), PRINTER);
    assert_eq!(u.parts.get(&u.target(&sheet, &attr_of(&sx, "control", "r:id")).1).unwrap(), CTRL_PROP.as_bytes());
    // Without notes the kept VML is written on its own, still without the old note.
    let vml = u.text(&u.target(&sheet, &attr_of(&sx, "legacyDrawing", "r:id")).1);
    assert!(vml.contains("_x0000_s1026") && !vml.contains("ObjectType=\"Note\""));
    assert!(!u.text("xl/worksheets/sheet1.xml").contains("pageSetup orientation=\"landscape\" r:id"));

    // Deleting the sheet drops its content; workbook-level content stays.
    wb.sheets.remove(1);
    wb.names.clear();
    let u = Unpacked::new(&write_xlsx(&wb).unwrap());
    u.assert_valid();
    for d in [PRINTER, OLE, EMF, QUERY_TABLE.as_bytes(), CTRL_PROP.as_bytes()] {
        assert!(u.find(d).is_empty());
    }
    for d in [CUSTOM_XML.as_bytes(), EXTERNAL_LINK.as_bytes(), CONNECTIONS.as_bytes(), CUSTOM_PROPS.as_bytes()] {
        assert_eq!(u.find(d).len(), 1);
    }
}

/// #324: an openpyxl-style workbook whose sheet carries printer settings.
#[test]
fn printer_settings_and_page_setup_id_survive_save() {
    let ct = r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="bin" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.printerSettings"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/></Types>"#;
    let bin: Vec<u8> = (0..=255u8).cycle().take(1024).collect();
    let pkg = super::minimal(
        r#"<sheetData><row r="1"><c r="A1"><v>42.1</v></c></row></sheetData><pageMargins left="0.75" right="0.75" top="1" bottom="1" header="0.5" footer="0.5"/><pageSetup orientation="landscape" r:id="rIdPs1"/>"#,
        &[("xl/worksheets/_rels/sheet1.xml.rels", &rels(&[("rIdPs1", "printerSettings", "../printerSettings/printerSettings1.bin")]))],
        "",
        "",
    );
    // Swap in content types that know `.bin`, and add the binary part.
    let mut u = Unpacked::new(&pkg);
    u.parts.insert("[Content_Types].xml".into(), ct.as_bytes().to_vec());
    u.parts.insert("xl/printerSettings/printerSettings1.bin".into(), bin.clone());
    let list: Vec<(&str, &[u8])> = u.parts.iter().map(|(k, v)| (k.as_str(), v.as_slice())).collect();
    let (wb, rep) = read_xlsx(&make_zip(&list)).unwrap();
    assert!(rep.warnings.iter().all(|w| !w.contains("printer")), "{:?}", rep.warnings);
    let out = Unpacked::new(&write_xlsx(&wb).unwrap());
    out.assert_valid();
    let sx = out.text("xl/worksheets/sheet1.xml");
    assert!(sx.contains("orientation=\"landscape\""));
    let (ty, target) = out.target("xl/worksheets/sheet1.xml", &attr_of(&sx, "pageSetup", "r:id"));
    assert!(ty.ends_with("/printerSettings"));
    assert_eq!(target, "xl/printerSettings/printerSettings1.bin");
    assert_eq!(out.parts.get(&target).unwrap(), &bin);
}

#[test]
fn damaged_kept_content_is_dropped_without_failing_the_read() {
    let ct = CONTENT_TYPES.replace("</Types>", r#"<Override PartName="/customXml/item2.xml" ContentType="application/xml"/></Types>"#);
    let workbook = format!(
        r#"<?xml version="1.0"?><workbook xmlns="{NS_MAIN}" xmlns:r="{NS_R}"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets><externalReferences><externalReference r:id="rId3"/><externalReference r:id="rId99"/></externalReferences></workbook>"#
    );
    let sheet = format!(
        r#"<?xml version="1.0"?><worksheet xmlns="{NS_MAIN}" xmlns:r="{NS_R}"><sheetData/><pageSetup r:id="rId1"/><controls><control shapeId="1" r:id="rId404" name="Broken"/></controls></worksheet>"#
    );
    let wb_rels = rels(&[
        ("rId1", "worksheet", "worksheets/sheet1.xml"),
        ("rId3", "externalLink", "externalLinks/externalLink1.xml"),
        // A missing part.
        ("rId4", "customXml", "../customXml/missing.xml"),
        // A cycle: item1 → itemProps1 → item1.
        ("rId5", "customXml", "../customXml/item1.xml"),
        // Kept content that points into a part GridCraft rewrites.
        ("rId6", "customXml", "../customXml/item2.xml"),
        // Damaged rels behind a kept part.
        ("rId7", "connections", "connections.xml"),
    ]);
    let cyclic = rels(&[("rId1", "customXmlProps", "itemProps1.xml")]);
    let back = rels(&[("rId1", "customXml", "item1.xml")]);
    let into_sheet = rels(&[("rId1", "worksheet", "../xl/worksheets/sheet1.xml")]);
    let sheet_rels = rels(&[("rId1", "printerSettings", "../printerSettings/missing.bin")]);
    let bytes = make_zip(&[
        ("[Content_Types].xml", ct.as_bytes()),
        ("_rels/.rels", super::ROOT_RELS.as_bytes()),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", wb_rels.as_bytes()),
        ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
        ("xl/worksheets/_rels/sheet1.xml.rels", sheet_rels.as_bytes()),
        ("xl/externalLinks/externalLink1.xml", EXTERNAL_LINK.as_bytes()),
        ("customXml/item1.xml", CUSTOM_XML.as_bytes()),
        ("customXml/_rels/item1.xml.rels", cyclic.as_bytes()),
        ("customXml/itemProps1.xml", b"<p/>"),
        ("customXml/_rels/itemProps1.xml.rels", back.as_bytes()),
        ("customXml/item2.xml", b"<q/>"),
        ("customXml/_rels/item2.xml.rels", into_sheet.as_bytes()),
        ("xl/connections.xml", CONNECTIONS.as_bytes()),
        ("xl/_rels/connections.xml.rels", b"<Relationships"),
    ]);
    let (wb, rep) = read_xlsx(&bytes).unwrap();
    let w = rep.warnings.join("\n");
    assert!(w.contains("customXml/missing.xml is missing"), "{w}");
    assert!(w.contains("refers to worksheet"), "{w}");
    assert!(w.contains("links to external workbooks were dropped"), "a link without a part drops all links: {w}");
    assert!(w.contains("printer settings were dropped"), "{w}");
    assert!(w.contains("dropped: the sheet has no VML drawing"), "{w}");
    let p = wb.preserved.as_deref().unwrap();
    // Only the cyclic custom XML pair is consistent enough to keep.
    let mut names: Vec<&str> = p.parts.iter().map(|q| q.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["customXml/item1.xml", "customXml/itemProps1.xml"]);
    assert!(p.elements.is_empty());
    assert!(wb.sheet(0).unwrap().preserved.is_none());
    let u = Unpacked::new(&write_xlsx(&wb).unwrap());
    u.assert_valid();
    assert!(!u.text("xl/workbook.xml").contains("externalReferences"));
}

#[test]
fn oversized_kept_parts_are_dropped() {
    let big = vec![0u8; crate::passthrough::MAX_KEPT_PART as usize + 1];
    let ct = CONTENT_TYPES;
    let wb_rels = rels(&[("rId1", "worksheet", "worksheets/sheet1.xml"), ("rId2", "customXml", "../customXml/item1.xml")]);
    let workbook = format!(
        r#"<?xml version="1.0"?><workbook xmlns="{NS_MAIN}" xmlns:r="{NS_R}"><sheets><sheet name="A" sheetId="1" r:id="rId1"/></sheets></workbook>"#
    );
    let sheet = format!(r#"<?xml version="1.0"?><worksheet xmlns="{NS_MAIN}"><sheetData/></worksheet>"#);
    let bytes = make_zip(&[
        ("[Content_Types].xml", ct.as_bytes()),
        ("_rels/.rels", super::ROOT_RELS.as_bytes()),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", wb_rels.as_bytes()),
        ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
        ("customXml/item1.xml", &big),
    ]);
    drop(big);
    let (wb, rep) = read_xlsx(&bytes).unwrap();
    assert!(wb.preserved.is_none());
    assert!(rep.warnings.iter().any(|w| w.contains("customXml") && w.contains("dropped")), "{:?}", rep.warnings);
}

#[test]
fn kept_content_types_are_escaped_on_save() {
    // A content type holding `"` and `<` (legal once unescaped by the XML reader) must not break
    // out of the attribute in the saved [Content_Types].xml.
    let hostile = r#"application/x-test&quot; PartName=&quot;/x&quot;/&gt;&lt;Default Extension=&quot;evil"#;
    let ct = CONTENT_TYPES.replace("application/vnd.openxmlformats-officedocument.customXmlProperties+xml", hostile);
    let (wb, rep) = read_xlsx(&rich_package_with(&ct, false)).unwrap();
    assert!(wb.preserved.is_some(), "{:?}", rep.warnings);
    let u = Unpacked::new(&write_xlsx(&wb).unwrap());
    let types = xml::parse(u.text("[Content_Types].xml").as_bytes()).unwrap();
    assert!(!types.kids("Default").any(|d| d.attr("Extension") == Some("evil")));
    let item = types.kids("Override").find(|o| o.attr("PartName") == Some("/customXml/itemProps1.xml")).unwrap();
    assert_eq!(item.attr("ContentType"), Some(r#"application/x-test" PartName="/x"/><Default Extension="evil"#));
    // And the saved file reads back.
    read_xlsx(&write_xlsx(&wb).unwrap()).unwrap();
}

#[test]
fn kept_vml_drawings_are_charged_to_the_kept_total() {
    // How much one sheet's kept content costs, with the default allowance.
    let one = {
        let (wb, rep) = read_xlsx(&rich_package()).unwrap();
        assert!(!rep.warnings.iter().any(|w| w.contains("dropped")), "{:?}", rep.warnings);
        wb.preserved.as_ref().map_or(0, |p| p.byte_len()) + wb.sheets.iter().filter_map(|s| s.preserved.as_ref()).map(|p| p.byte_len()).sum::<usize>()
    };
    // Two sheets share one VML drawing: each would keep its own copy. With room for one sheet's
    // content only, the second sheet's VML (and the controls drawn from it) is dropped.
    crate::passthrough::KEPT_TOTAL_FOR_TEST.with(|c| c.set(one + VML.len() / 2));
    let read = read_xlsx(&rich_package_with(CONTENT_TYPES, true));
    crate::passthrough::KEPT_TOTAL_FOR_TEST.with(|c| c.set(crate::passthrough::MAX_KEPT_TOTAL));
    let (wb, rep) = read.unwrap();
    let kept = wb.preserved.as_ref().map_or(0, |p| p.byte_len())
        + wb.sheets.iter().filter_map(|s| s.preserved.as_ref()).map(|p| p.byte_len()).sum::<usize>();
    assert!(kept <= one + VML.len() / 2, "kept {kept} bytes");
    assert!(wb.sheet(0).unwrap().preserved.as_ref().is_some_and(|p| p.vml.is_some()));
    assert!(!wb.sheet(1).unwrap().preserved.as_ref().is_some_and(|p| p.vml.is_some()));
    assert!(rep.warnings.iter().any(|w| w.contains("\"Other\"") && w.contains("too large")), "{:?}", rep.warnings);
    // Both sheets keep their drawings when there is room.
    let (wb, _) = read_xlsx(&rich_package_with(CONTENT_TYPES, true)).unwrap();
    assert!(wb.sheets.iter().all(|s| s.preserved.as_ref().is_some_and(|p| p.vml.is_some())));
}
