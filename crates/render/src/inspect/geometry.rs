//! Page size from an already-open COS document.
//!
//! The rules match the renderer's page geometry: inherited MediaBox, CropBox and Rotate,
//! a page-local UserUnit, the A4 default, the crop/media intersection, and the 90/270
//! axis swap. Building a renderer page also loads resources and content for every page,
//! which is what made a 100,000-page file take seconds and hundreds of megabytes to open.

use std::collections::HashSet;

use pdfcraft_cos::{Dict, Document, ObjRef, Object};

use super::PageInfo;

const MAX_DEPTH: usize = 256;
const MAX_NODES: u32 = 1_000_000;
const MAX_USER_UNIT: f64 = 75_000.0;
const NEARLY_ZERO: f32 = 1.0 / 4096.0;
const POINTS_PER_MM: f64 = 72.0 / 25.4;
const A4: [f64; 4] = [0.0, 0.0, 210.0 * POINTS_PER_MM, 297.0 * POINTS_PER_MM];

#[derive(Clone, Copy, Default)]
struct Inherit {
    media: Option<[f64; 4]>,
    crop: Option<[f64; 4]>,
    rotate: Option<i64>,
}

/// Displayed page boxes, in document order. `None` when the tree is missing, the root has
/// no kids, or the walk passes its node budget. The caller then uses the renderer parser,
/// which also recovers pages by scanning.
struct Walk<'a> {
    doc: &'a Document,
    seen: HashSet<ObjRef>,
    nodes: u32,
    out: Vec<PageInfo>,
}

pub(super) fn page_infos(doc: &Document) -> Option<Vec<PageInfo>> {
    let root = doc.root()?;
    let catalog = doc.get(root);
    let catalog = catalog.as_dict()?;
    let pages = catalog.get(b"Pages")?;
    let mut walk = Walk { doc, seen: HashSet::new(), nodes: 0, out: Vec::new() };
    let finished = if let Some(id) = pages.as_ref() {
        walk.root(id)
    } else {
        let object = doc.resolve(pages);
        let dict = object.as_dict()?;
        walk.pages(dict, None, Inherit::default(), 0)
    };
    (finished && !walk.out.is_empty()).then_some(walk.out)
}

impl Walk<'_> {
    fn root(&mut self, id: ObjRef) -> bool {
        if !self.budget() {
            return false;
        }
        self.seen.insert(id);
        let object = self.doc.get(id);
        let Some(dict) = object.as_dict() else { return false };
        self.pages(dict, Some(id), Inherit::default(), 0)
    }

    /// A pages node. `false` means the walk stopped because it ran out of budget, or the root
    /// has no kids (the renderer then scans the file for pages).
    fn pages(&mut self, dict: &Dict, id: Option<ObjRef>, mut inherited: Inherit, depth: usize) -> bool {
        if let Some(id) = id {
            self.seen.insert(id);
        }
        inherit_from(self.doc, dict, &mut inherited);
        let Some(kids) = kids_of(self.doc, dict) else {
            // The root must have kids. A nested node without them contributes nothing.
            return depth != 0;
        };
        if kids.is_empty() && depth == 0 {
            return false;
        }
        for kid in kids {
            if !self.budget() || !self.kid(&kid, inherited, depth + 1) {
                return false;
            }
        }
        true
    }

    fn kid(&mut self, kid: &Object, inherited: Inherit, depth: usize) -> bool {
        if depth > MAX_DEPTH {
            return true;
        }
        let (id, object) = if let Some(id) = kid.as_ref() {
            let object = self.doc.get(id);
            if self.seen.contains(&id) && is_pages_dict(object.as_dict()) {
                return true;
            }
            (Some(id), object)
        } else {
            (None, self.doc.resolve(kid))
        };
        let Some(dict) = object.as_dict() else { return true };
        if is_pages_dict(Some(dict)) {
            self.pages(dict, id, inherited, depth)
        } else {
            let page = page_from(self.doc, dict, inherited, self.out.len());
            self.out.push(page);
            true
        }
    }

    fn budget(&mut self) -> bool {
        self.nodes = self.nodes.saturating_add(1);
        self.nodes <= MAX_NODES
    }
}

fn is_pages_dict(dict: Option<&Dict>) -> bool {
    dict.and_then(|d| d.name(b"Type")) == Some(b"Pages")
}

fn inherit_from(doc: &Document, dict: &Dict, inherited: &mut Inherit) {
    if let Some(media) = rect_key(doc, dict, b"MediaBox") {
        inherited.media = Some(media);
    }
    if let Some(crop) = rect_key(doc, dict, b"CropBox") {
        inherited.crop = Some(crop);
    }
    if let Some(rotate) = int_key(doc, dict, b"Rotate") {
        inherited.rotate = Some(rotate);
    }
}

fn kids_of(doc: &Document, dict: &Dict) -> Option<Vec<Object>> {
    let raw = dict.get(b"Kids")?;
    doc.resolve(raw).as_array().cloned()
}

fn page_from(doc: &Document, dict: &Dict, inherited: Inherit, index: usize) -> PageInfo {
    let media = rect_key(doc, dict, b"MediaBox").or(inherited.media).unwrap_or(A4);
    let crop = rect_key(doc, dict, b"CropBox").or(inherited.crop).unwrap_or(media);
    let rotation = int_key(doc, dict, b"Rotate").or(inherited.rotate).unwrap_or(0);
    let user_unit = dict
        .get(b"UserUnit")
        .and_then(|value| doc.resolve(value).as_f64())
        .filter(|unit| *unit >= 1.0)
        .map_or(1.0, |unit| unit.min(MAX_USER_UNIT) as f32);
    let (width, height, crop, rotation) = display(media, crop, rotation, user_unit);
    PageInfo { width, height, label: (index + 1).to_string(), crop, rotation }
}

fn display(media: [f64; 4], crop: [f64; 4], rotation: i64, user_unit: f32) -> (f32, f32, [f32; 4], u16) {
    let x0 = crop[0].max(media[0]);
    let y0 = crop[1].max(media[1]);
    let x1 = crop[2].min(media[2]).max(x0);
    let y1 = crop[3].min(media[3]).max(y0);
    let crop = [x0 as f32, y0 as f32, x1 as f32, y1 as f32];
    let width = x1 - x0;
    let height = y1 - y0;
    let (base_width, base_height) = if (width as f32).abs() <= NEARLY_ZERO || (height as f32).abs() <= NEARLY_ZERO {
        (A4[2] as f32, A4[3] as f32)
    } else {
        let mut raw_floor = 1.0 / user_unit;
        if raw_floor * user_unit < 1.0 {
            raw_floor = raw_floor.next_up();
        }
        let raw_floor = f64::from(raw_floor);
        (width.max(raw_floor) as f32, height.max(raw_floor) as f32)
    };
    let (mut width, mut height) = (base_width * user_unit, base_height * user_unit);
    let rotation = match rotation.rem_euclid(360) {
        90 => 90,
        180 => 180,
        270 => 270,
        _ => 0,
    };
    if rotation == 90 || rotation == 270 {
        std::mem::swap(&mut width, &mut height);
    }
    let (width, height) = if width.is_finite() && height.is_finite() && width >= 1.0 && height >= 1.0 { (width, height) } else { (612.0, 792.0) };
    (width, height, crop, rotation)
}

fn rect_key(doc: &Document, dict: &Dict, key: &[u8]) -> Option<[f64; 4]> {
    rect_of(doc, dict.get(key)?)
}

fn rect_of(doc: &Document, value: &Object) -> Option<[f64; 4]> {
    let object = doc.resolve(value);
    let array = object.as_array()?;
    let mut numbers = array.iter().map(|item| number(doc, item));
    let x0 = numbers.next()??;
    let y0 = numbers.next()??;
    let x1 = numbers.next()??;
    let y1 = numbers.next()??;
    Some([x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)])
}

fn number(doc: &Document, value: &Object) -> Option<f64> {
    doc.resolve(value).as_f64().map(|value| f64::from(value as f32))
}

fn int_key(doc: &Document, dict: &Dict, key: &[u8]) -> Option<i64> {
    dict.get(key).and_then(|value| doc.resolve(value).as_int())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pdfcraft_cos::{Dict, Document, Object, SaveOptions, write_full};

    use super::super::inspect;
    use super::page_infos;

    fn save(doc: &Document) -> Vec<u8> {
        write_full(doc, &SaveOptions { object_streams: false, ..SaveOptions::default() }).expect("write")
    }

    fn page(doc: &mut Document, parent: pdfcraft_cos::ObjRef, extras: Dict) -> Object {
        let mut dict = Dict::new();
        dict.set(b"Type".to_vec(), Object::name("Page"));
        dict.set(b"Parent".to_vec(), Object::Ref(parent));
        for (key, value) in extras.iter() {
            dict.set(key.clone(), value.clone());
        }
        Object::Ref(doc.add(dict))
    }

    fn tree(pages_extra: Dict, page_extra: Dict) -> Vec<u8> {
        let mut doc = Document::new_empty();
        let pages = doc.root().and_then(|root| doc.get(root).as_dict().and_then(|dict| dict.reference(b"Pages"))).expect("pages");
        doc.update_dict(pages, |dict| {
            for (key, value) in pages_extra.iter() {
                dict.set(key.clone(), value.clone());
            }
        })
        .expect("pages");
        let kid = page(&mut doc, pages, page_extra);
        doc.update_dict(pages, |dict| {
            dict.set(b"Kids".to_vec(), Object::Array(vec![kid]));
            dict.set(b"Count".to_vec(), Object::Int(1));
        })
        .expect("kids");
        save(&doc)
    }

    fn box_array(values: [f64; 4]) -> Object {
        Object::Array(values.into_iter().map(Object::from).collect())
    }

    fn matches_renderer(bytes: Vec<u8>) {
        let bytes = Arc::new(bytes);
        let renderer = super::super::core::inspect_with_renderer(bytes.clone(), None).expect("renderer inspect");
        let opened = Document::open(bytes.clone()).expect("cos");
        let fast = inspect(bytes, None).expect("fast inspect");
        let ours = page_infos(&opened).expect("geometry");
        assert_eq!(fast.pages.len(), renderer.pages.len());
        assert_eq!(fast.pdf_version, renderer.pdf_version);
        assert_eq!(ours.len(), renderer.pages.len());
        for ((ours, fast), renderer) in ours.iter().zip(&fast.pages).zip(&renderer.pages) {
            assert_eq!(ours.rotation, renderer.rotation);
            assert_eq!(fast.rotation, renderer.rotation);
            assert_eq!(ours.label, renderer.label);
            assert_eq!(fast.label, renderer.label);
            for ((ours, fast), renderer) in ours.crop.iter().zip(&fast.crop).zip(&renderer.crop) {
                assert!((ours - renderer).abs() < 0.01, "crop {ours} vs {renderer}");
                assert!((fast - renderer).abs() < 0.01, "fast crop {fast} vs {renderer}");
            }
            assert!((ours.width - renderer.width).abs() < 0.01, "width {} vs {}", ours.width, renderer.width);
            assert!((ours.height - renderer.height).abs() < 0.01, "height {} vs {}", ours.height, renderer.height);
            assert!((fast.width - renderer.width).abs() < 0.01, "fast width {} vs {}", fast.width, renderer.width);
            assert!((fast.height - renderer.height).abs() < 0.01, "fast height {} vs {}", fast.height, renderer.height);
        }
    }

    #[test]
    fn geometry_matches_the_renderer_on_inherited_crop_rotation_and_user_unit() {
        let mut plain = Dict::new();
        plain.set(b"MediaBox".to_vec(), box_array([0.0, 0.0, 200.0, 300.0]));
        matches_renderer(tree(Dict::new(), plain));

        let mut inherited = Dict::new();
        inherited.set(b"MediaBox".to_vec(), box_array([0.0, 0.0, 400.0, 500.0]));
        matches_renderer(tree(inherited, Dict::new()));

        let mut cropped = Dict::new();
        cropped.set(b"MediaBox".to_vec(), box_array([0.0, 0.0, 400.0, 500.0]));
        cropped.set(b"CropBox".to_vec(), box_array([10.0, 20.0, 110.0, 220.0]));
        matches_renderer(tree(Dict::new(), cropped));

        let mut turned = Dict::new();
        turned.set(b"MediaBox".to_vec(), box_array([0.0, 0.0, 200.0, 300.0]));
        turned.set(b"Rotate".to_vec(), Object::Int(90));
        matches_renderer(tree(Dict::new(), turned));

        let mut parent_turn = Dict::new();
        parent_turn.set(b"Rotate".to_vec(), Object::Int(-90));
        let mut sized = Dict::new();
        sized.set(b"MediaBox".to_vec(), box_array([0.0, 0.0, 200.0, 300.0]));
        matches_renderer(tree(parent_turn, sized));

        let mut scaled = Dict::new();
        scaled.set(b"MediaBox".to_vec(), box_array([0.0, 0.0, 200.0, 300.0]));
        scaled.set(b"UserUnit".to_vec(), Object::Real(2.0));
        matches_renderer(tree(Dict::new(), scaled));
    }
}
