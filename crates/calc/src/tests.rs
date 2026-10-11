use gridcraft_core::{CellError, CellRef, Value};
use gridcraft_model::{Cell, Formula, Workbook};

use crate::Calc;

fn c(s: &str) -> CellRef {
    CellRef::parse(s).unwrap()
}

struct T {
    wb: Workbook,
    calc: Calc,
}

impl T {
    fn new() -> T {
        T { wb: Workbook::new(), calc: Calc::new() }
    }
    fn set(&mut self, at: &str, input: &str) {
        let cell = if input.starts_with('=') {
            Cell::formula(Formula::new(input))
        } else {
            Cell::value(gridcraft_core::parse::parse_input(input, self.wb.date_system).value)
        };
        self.wb.sheet_mut(0).unwrap().cells.set(c(at), cell);
        self.calc.cells_changed(&mut self.wb, &[(0, c(at))]);
    }
    fn get(&self, at: &str) -> Value {
        self.wb.sheet(0).unwrap().value(c(at))
    }
    fn num(&self, at: &str) -> f64 {
        self.get(at).as_f64().unwrap_or_else(|| panic!("{at} = {:?}", self.get(at)))
    }
}

#[test]
fn spill_range_operator() {
    let mut t = T::new();
    t.set("A1", "3");
    t.set("B1", "=SEQUENCE(A1)");
    t.set("C1", "=SUM(B1#)");
    t.set("C2", "=ROWS(B1#)");
    assert_eq!((t.num("C1"), t.num("C2")), (6.0, 3.0));
    // Growing and shrinking the spill recalculates its users.
    t.set("A1", "5");
    assert_eq!((t.num("C1"), t.num("C2")), (15.0, 5.0));
    t.set("A1", "2");
    assert_eq!((t.num("C1"), t.num("C2")), (3.0, 2.0));
    // A blocked spill, or a cell that doesn't spill, gives #REF!.
    t.set("B2", "x");
    assert_eq!(t.get("C1"), Value::Error(CellError::Ref));
    t.set("B2", "");
    assert_eq!(t.num("C1"), 3.0);
    t.set("D1", "=SUM(A1#)");
    assert_eq!(t.get("D1"), Value::Error(CellError::Ref));
    // Through a defined name.
    t.wb.names.push(gridcraft_model::DefinedName {
        name: "Seq".into(),
        scope: None,
        formula: "Sheet1!$B$1#".into(),
        comment: String::new(),
        hidden: false,
    });
    t.set("E1", "=SUM(Seq)*10");
    assert_eq!(t.num("E1"), 30.0);
    t.set("A1", "4");
    assert_eq!(t.num("E1"), 100.0);
}

#[test]
fn arithmetic_and_dependencies() {
    let mut t = T::new();
    t.set("A1", "2");
    t.set("A2", "3");
    t.set("A3", "=A1*A2+1");
    assert_eq!(t.num("A3"), 7.0);
    t.set("A1", "10");
    assert_eq!(t.num("A3"), 31.0);
    t.set("B1", "=SUM(A1:A3)");
    assert_eq!(t.num("B1"), 44.0);
    t.set("A2", "0");
    assert_eq!(t.num("A3"), 1.0);
    assert_eq!(t.num("B1"), 11.0);
}

#[test]
fn chains_out_of_order() {
    let mut t = T::new();
    t.set("A5", "=A4+1");
    t.set("A4", "=A3+1");
    t.set("A3", "=A2+1");
    t.set("A2", "=A1+1");
    t.set("A1", "1");
    assert_eq!(t.num("A5"), 5.0);
}

#[test]
fn long_chain() {
    let mut t = T::new();
    t.set("A1", "1");
    for r in 2..=3000 {
        t.set(&format!("A{r}"), &format!("=A{}+1", r - 1));
    }
    assert_eq!(t.num("A3000"), 3000.0);
    t.set("A1", "10");
    assert_eq!(t.num("A3000"), 3009.0);
}

#[test]
fn errors_and_cycles() {
    let mut t = T::new();
    t.set("A1", "=1/0");
    assert_eq!(t.get("A1"), Value::Error(CellError::Div0));
    t.set("B1", "=B2");
    t.set("B2", "=B1");
    assert!(t.get("B1").is_error());
    t.set("C1", "=NOSUCHFN(1)");
    assert_eq!(t.get("C1"), Value::Error(CellError::Name));
    t.set("C2", "=1+");
    assert_eq!(t.get("C2"), Value::Error(CellError::Name));
}

#[test]
fn operators() {
    let mut t = T::new();
    t.set("A1", "=\"a\"&1&TRUE");
    assert_eq!(t.get("A1"), Value::from("a1TRUE"));
    t.set("A2", "=-2^2");
    assert_eq!(t.num("A2"), 4.0);
    t.set("A3", "=50%");
    assert_eq!(t.num("A3"), 0.5);
    t.set("A4", "=\"abc\"=\"ABC\"");
    assert_eq!(t.get("A4"), Value::Bool(true));
    t.set("A5", "=\"5\"+1");
    assert_eq!(t.num("A5"), 6.0);
    t.set("A6", "=\"x\"+1");
    assert_eq!(t.get("A6"), Value::Error(CellError::Value));
    t.set("A7", "=B99");
    assert_eq!(t.num("A7"), 0.0);
}

#[test]
fn blocked_spill_reads_as_spill_error() {
    // Calculated in the same pass, a formula over a blocked spill read the array that couldn't
    // spill.
    let mut t = T::new();
    let s = t.wb.sheet_mut(0).unwrap();
    for a in ["A1", "B1", "C1", "A2", "C2", "A3", "B3", "C3"] {
        s.cells.set(c(a), Cell::value(Value::Number(1.0)));
    }
    s.cells.set(c("B2"), Cell::formula(Formula::new("=SEQUENCE(2,2,9)")));
    s.cells.set(c("E1"), Cell::formula(Formula::new("=SUM(A1:C3)")));
    t.calc.recalc_all(&mut t.wb);
    assert_eq!(t.get("B2"), Value::Error(CellError::Spill));
    assert_eq!(t.get("E1"), Value::Error(CellError::Spill));
    assert_eq!(t.num("C3"), 1.0);
}

#[test]
fn blocked_spill_read_through_offset() {
    // The reader's reference is only known while evaluating, so it isn't in the dependency graph.
    let mut t = T::new();
    t.set("E4", "5");
    t.set("E8", "=SUM(OFFSET(E2,0,0,2,2))");
    t.set("E3", "=SEQUENCE(2,2,3)");
    t.calc.recalc_all(&mut t.wb);
    assert_eq!(t.get("E3"), Value::Error(CellError::Spill));
    assert_eq!(t.get("E8"), Value::Error(CellError::Spill));
}

#[test]
fn spill_into_the_old_area_of_a_blocked_neighbour() {
    // C7's array becomes blocked by C8, whose array then takes over C7's old area.
    let mut t = T::new();
    t.set("C7", "=SEQUENCE(2,2)");
    t.set("C8", "=SEQUENCE(2,2)");
    assert_eq!(t.get("C7"), Value::Error(CellError::Spill));
    assert_eq!([t.num("C8"), t.num("D8"), t.num("C9"), t.num("D9")], [1.0, 2.0, 3.0, 4.0]);
}

#[test]
fn arrays_do_not_spill_in_tables() {
    // Excel: spilled array formulas aren't supported in tables.
    let mut t = T::new();
    t.wb.sheet_mut(0).unwrap().tables.push(gridcraft_model::Table {
        id: 1,
        name: "Table1".into(),
        range: gridcraft_core::RangeRef::parse("B3:C6").unwrap(),
        header_row: true,
        totals_row: false,
        columns: vec![],
        style: String::new(),
        banded_rows: false,
        banded_cols: false,
        first_col: false,
        last_col: false,
        filter_button: false,
    });
    t.set("C4", "=SEQUENCE(2)");
    t.set("E1", "=SEQUENCE(4)");
    t.set("H1", "=SUM(C4:C5)");
    assert_eq!(t.get("C4"), Value::Error(CellError::Spill));
    assert_eq!(t.get("C5"), Value::Empty);
    assert_eq!(t.get("H1"), Value::Error(CellError::Spill));
    assert_eq!(t.num("E4"), 4.0);
}

#[test]
fn ifs_and_lazy() {
    let mut t = T::new();
    t.set("A1", "5");
    t.set("B1", "=IF(A1>3,\"big\",1/0)");
    assert_eq!(t.get("B1"), Value::from("big"));
    t.set("B2", "=IFERROR(1/0,\"oops\")");
    assert_eq!(t.get("B2"), Value::from("oops"));
    t.set("B3", "=CHOOSE(2,\"a\",\"b\",\"c\")");
    assert_eq!(t.get("B3"), Value::from("b"));
    t.set("B4", "=SWITCH(A1,1,\"one\",5,\"five\",\"other\")");
    assert_eq!(t.get("B4"), Value::from("five"));
}

#[test]
fn references_functions() {
    let mut t = T::new();
    for (i, v) in [10, 20, 30, 40].iter().enumerate() {
        t.set(&format!("A{}", i + 1), &v.to_string());
    }
    t.set("B1", "=ROW(A3)");
    assert_eq!(t.num("B1"), 3.0);
    t.set("B2", "=ROWS(A1:A4)*COLUMNS(A1:C1)");
    assert_eq!(t.num("B2"), 12.0);
    t.set("B3", "=SUM(OFFSET(A1,1,0,2,1))");
    assert_eq!(t.num("B3"), 50.0);
    t.set("B4", "=INDIRECT(\"A\"&4)");
    assert_eq!(t.num("B4"), 40.0);
    t.set("B5", "=SUM(A1:INDEX(A1:A4,3))");
    assert_eq!(t.num("B5"), 60.0);
    t.set("B6", "=SUM(A:A)");
    assert_eq!(t.num("B6"), 100.0);
    t.set("A2", "25");
    assert_eq!(t.num("B3"), 55.0);
    assert_eq!(t.num("B6"), 105.0);
}

#[test]
fn spills() {
    let mut t = T::new();
    t.set("A1", "=SEQUENCE(3)");
    assert_eq!(t.num("A1"), 1.0);
    assert_eq!(t.num("A3"), 3.0);
    t.set("B1", "=SUM(A1:A3)");
    assert_eq!(t.num("B1"), 6.0);
    t.set("A2", "x");
    assert_eq!(t.get("A1"), Value::Error(CellError::Spill));
    t.set("A2", "");
    assert_eq!(t.num("A3"), 3.0);
    t.set("C1", "={1,2;3,4}*10");
    assert_eq!(t.num("D2"), 40.0);
}

#[test]
fn references_keep_their_size_past_the_used_range() {
    let mut t = T::new();
    t.set("A1", "5");
    t.set("A2", "6");
    t.set("C1", "=ROWS(A1:A10*1)");
    t.set("C2", "=SUMPRODUCT(--(A1:A10=\"\"))");
    t.set("C3", "=COUNTBLANK(A1:A10)");
    t.set("C4", "=COUNTIF(A1:A10,\"\")");
    t.set("C5", "=COLUMNS(TRANSPOSE(A1:A10))");
    for (at, want) in [("C1", 10.0), ("C2", 8.0), ("C3", 8.0), ("C4", 8.0), ("C5", 10.0)] {
        assert_eq!(t.num(at), want, "{at}");
    }
    // Whole columns are still read up to the used range.
    t.set("D1", "=ROWS(FILTER(A:A,A:A<>\"\"))");
    assert_eq!(t.num("D1"), 2.0);
}

#[test]
fn spilled_blank_cells_show_zero() {
    let mut t = T::new();
    t.set("A1", "5");
    t.set("A3", "7");
    t.set("C1", "=A1:A4");
    assert_eq!(t.get("C2"), Value::Number(0.0));
    assert_eq!(t.get("C4"), Value::Number(0.0));
    t.set("D1", "=COUNT(C1:C4)");
    assert_eq!(t.num("D1"), 4.0);
    t.set("E1", "=A2:A3");
    assert_eq!(t.get("E1"), Value::Number(0.0));
    t.set("F1", "=SORT(A1:A3)");
    assert_eq!(t.get("F3"), Value::Number(0.0));
}

#[test]
fn let_lambda() {
    let mut t = T::new();
    t.set("A1", "=LET(x,2,y,3,x*y)");
    assert_eq!(t.num("A1"), 6.0);
    t.set("A2", "=LAMBDA(a,b,a+b)(2,3)");
    assert_eq!(t.num("A2"), 5.0);
    t.set("A3", "=REDUCE(0,{1,2,3},LAMBDA(acc,v,acc+v))");
    assert_eq!(t.num("A3"), 6.0);
    t.set("A4", "=SUM(MAP({1,2,3},LAMBDA(v,v*v)))");
    assert_eq!(t.num("A4"), 14.0);
}

#[test]
fn names_and_sheets() {
    let mut t = T::new();
    t.wb.names.push(gridcraft_model::DefinedName { name: "Rate".into(), scope: None, formula: "0.5".into(), comment: String::new(), hidden: false });
    t.wb.sheets.push(std::sync::Arc::new(gridcraft_model::Sheet::new("Data")));
    t.wb.sheet_mut(1).unwrap().set_value(c("A1"), Value::Number(8.0));
    t.calc.rebuild(&t.wb);
    t.set("A1", "=Rate*Data!A1");
    assert_eq!(t.num("A1"), 4.0);
    t.wb.sheet_mut(1).unwrap().set_value(c("A1"), Value::Number(10.0));
    t.calc.cells_changed(&mut t.wb, &[(1, c("A1"))]);
    assert_eq!(t.num("A1"), 5.0);
}

#[test]
fn legacy_array_formulas_fill_their_range() {
    let mut t = T::new();
    t.set("A1", "1");
    t.set("A2", "2");
    let cse = |t: &mut T, at: &str, formula: &str, range: &str| {
        let mut f = Formula::new(formula);
        f.array = gridcraft_core::RangeRef::parse(range);
        t.wb.sheet_mut(0).unwrap().cells.set(c(at), Cell::formula(f));
        t.calc.cells_changed(&mut t.wb, &[(0, c(at))]);
    };
    // Past the result: #N/A. A single column repeats across the range.
    cse(&mut t, "C1", "=A1:A2*2", "C1:D3");
    cse(&mut t, "F1", "=A1:A2*2", "F1");
    cse(&mut t, "G1", "=5", "G1:G2");
    assert_eq!(t.num("C2"), 4.0);
    assert_eq!(t.num("D2"), 4.0);
    assert_eq!(t.get("C3"), Value::Error(CellError::NA));
    // A smaller range shows only part of the result and never spills.
    assert_eq!(t.num("F1"), 2.0);
    assert_eq!(t.get("F2"), Value::Empty);
    assert_eq!(t.num("G2"), 5.0);
    // A range too large to fill is an error, not an allocation of every cell.
    cse(&mut t, "H1", "=1", "H1:XFD1048576");
    assert_eq!(t.get("H1"), Value::Error(CellError::Num));
}

#[test]
fn text_function() {
    let mut t = T::new();
    t.set("A1", "=TEXT(1234.5,\"#,##0.00\")");
    assert_eq!(t.get("A1"), Value::from("1,234.50"));
}

#[test]
fn subtotal_skips_hidden() {
    let mut t = T::new();
    t.set("A1", "1");
    t.set("A2", "2");
    t.set("A3", "3");
    t.wb.sheet_mut(0).unwrap().rows.insert(1, gridcraft_model::LineInfo { hidden: true, ..Default::default() });
    t.set("B1", "=SUBTOTAL(109,A1:A3)");
    t.set("B2", "=SUBTOTAL(9,A1:A3)");
    assert_eq!(t.num("B1"), 4.0);
    assert_eq!(t.num("B2"), 6.0);
}

#[test]
fn recalc_all_after_load() {
    let mut t = T::new();
    let s = t.wb.sheet_mut(0).unwrap();
    s.cells.set(c("A1"), Cell::value(Value::Number(4.0)));
    s.cells.set(c("A2"), Cell::formula(Formula::new("=A1*2")));
    s.cells.set(c("A3"), Cell::formula(Formula::new("=A2+A1")));
    t.calc.recalc_all(&mut t.wb);
    assert_eq!(t.num("A3"), 12.0);
}

#[test]
fn sumif_sum_range_takes_the_criteria_size() {
    // Excel adds up the range that starts at sum_range's top-left cell with the criteria range's
    // height and width, whatever size sum_range has.
    let mut t = T::new();
    for (r, (k, v)) in [("a", 1), ("b", 2), ("a", 3)].iter().enumerate() {
        t.set(&format!("A{}", r + 1), k);
        t.set(&format!("B{}", r + 1), &v.to_string());
    }
    t.set("D1", "=SUMIF(A1:A3,\"a\",B1)");
    assert_eq!(t.num("D1"), 4.0);
    t.set("D2", "=AVERAGEIF(A1:A3,\"a\",B1)");
    assert_eq!(t.num("D2"), 2.0);
    t.set("D3", "=SUMIF(A1:A3,\"a\",B1:B2)");
    assert_eq!(t.num("D3"), 4.0);
    t.set("D4", "=SUMIF(A1:A3,\"a\",B1:F1)");
    assert_eq!(t.num("D4"), 4.0);
    t.set("D5", "=SUMIF(A1:A3,\"a\",B1:B99)");
    assert_eq!(t.num("D5"), 4.0);
    t.set("D6", "=SUMIF(A:A,\"a\",B1)");
    assert_eq!(t.num("D6"), 4.0);
    t.set("D7", "=SUMIF(A1:A3,\"a\",$B$2)");
    assert_eq!(t.num("D7"), 2.0); // B2:B4: rows 1 and 3 match → B2 + B4 (empty)
    t.set("D8", "=SUMIF(A1:A3,\"a\")");
    assert_eq!(t.num("D8"), 0.0);
    // A horizontal criteria range resizes across.
    t.set("H1", "1");
    t.set("I1", "0");
    t.set("J1", "1");
    t.set("H2", "10");
    t.set("I2", "20");
    t.set("J2", "30");
    t.set("D9", "=SUMIF(H1:J1,1,H2)");
    assert_eq!(t.num("D9"), 40.0);
    // The resized cells are precedents: editing one recalculates.
    t.set("B3", "5");
    assert_eq!(t.num("D1"), 6.0);
    assert_eq!(t.num("D2"), 3.0);
    assert_eq!(t.num("D3"), 6.0);
    assert_eq!(t.num("D4"), 6.0);
    assert_eq!(t.num("D6"), 6.0);
    // SUMIFS and AVERAGEIFS don't resize: different sizes are #VALUE!.
    t.set("D11", "=SUMIFS(B1,A1:A3,\"a\")");
    assert_eq!(t.get("D11"), Value::Error(CellError::Value));
    t.set("D12", "=AVERAGEIFS(B1:B2,A1:A3,\"a\")");
    assert_eq!(t.get("D12"), Value::Error(CellError::Value));
    t.set("D13", "=SUMIFS(B1:B3,A1:A3,\"a\")");
    assert_eq!(t.num("D13"), 6.0);
}

#[test]
fn sumif_resized_sum_range_stops_at_the_sheet_edge() {
    let mut t = T::new();
    t.set("A1", "a");
    t.set("A2", "a");
    t.set("B1048575", "5");
    t.set("B1048576", "7");
    t.set("C1", "=SUMIF(A1:A2,\"a\",B1048576)");
    assert_eq!(t.num("C1"), 7.0);
    t.set("C2", "=SUMIF(A1:A2,\"a\",B1048575)");
    assert_eq!(t.num("C2"), 12.0);
    t.set("C3", "=SUMIF(A1:B1,\"a\",XFD1)");
    assert_eq!(t.num("C3"), 0.0);
}

#[test]
fn sumif_resized_sum_range_on_another_sheet_and_through_a_name() {
    let mut t = T::new();
    t.wb.sheets.push(std::sync::Arc::new(gridcraft_model::Sheet::new("Data")));
    t.wb.names.push(gridcraft_model::DefinedName {
        name: "Amounts".into(),
        scope: None,
        formula: "Data!$B$1".into(),
        comment: String::new(),
        hidden: false,
    });
    for (r, k) in ["a", "b", "a"].iter().enumerate() {
        t.set(&format!("A{}", r + 1), k);
        t.wb.sheet_mut(1).unwrap().set_value(CellRef::new(r as u32, 1), Value::Number((r + 1) as f64));
    }
    t.calc.rebuild(&t.wb);
    t.set("C1", "=SUMIF(A1:A3,\"a\",Data!B1)");
    assert_eq!(t.num("C1"), 4.0);
    t.set("C2", "=AVERAGEIF(A1:A3,\"a\",Amounts)");
    assert_eq!(t.num("C2"), 2.0);
    t.wb.sheet_mut(1).unwrap().set_value(c("B3"), Value::Number(9.0));
    t.calc.cells_changed(&mut t.wb, &[(1, c("B3"))]);
    assert_eq!(t.num("C1"), 10.0);
    assert_eq!(t.num("C2"), 5.0);
}

// ---- Excel's snap to zero (suite: cancel-ulps, cancel-band, decimal-sum, excel-probe) ----

impl T {
    /// Stores an exact double, bypassing input parsing.
    fn put(&mut self, at: &str, x: f64) {
        self.wb.sheet_mut(0).unwrap().cells.set(c(at), Cell::value(Value::Number(x)));
        self.calc.cells_changed(&mut self.wb, &[(0, c(at))]);
    }
}

const ULP1: f64 = f64::EPSILON; // the spacing of doubles just above 1.0

#[test]
fn a_bare_root_add_or_sub_snaps_a_near_cancellation_to_zero() {
    let mut t = T::new();
    t.put("A1", 1.0);
    t.put("B1", 1.0 + ULP1); // one ulp apart: Excel shows 0, not -2.22e-16
    t.set("C1", "=A1-B1");
    t.set("C2", "=B1-A1");
    t.set("C3", "=A1+-B1");
    t.set("C4", "=0+A1-B1"); // the end of =0+A-B snaps too
    t.set("C5", "=SUM(A1,-B1)*1"); // the last addition of a SUM snaps even when SUM isn't the root
    t.put("D1", -(1.0 + ULP1));
    t.set("C6", "=SUM(A1,D1)");
    t.set("C7", "=A1+D1");
    for at in ["C1", "C2", "C3", "C4", "C5", "C6", "C7"] {
        assert_eq!(t.get(at), Value::Number(0.0), "{at}");
    }
    // The classic: 0.1+0.2-0.3 is 0 in Excel (5.55e-17 in IEEE).
    t.put("E1", 0.1);
    t.put("E2", 0.2);
    t.put("E3", 0.3);
    t.set("E4", "=E1+E2-E3");
    assert_eq!(t.get("E4"), Value::Number(0.0));
}

#[test]
fn the_snap_threshold_is_two_to_the_e_minus_49_of_the_left_operand() {
    let mut t = T::new();
    t.put("A1", 1.0);
    // 7 ulps below the threshold of 8 ulps (2^-49): snaps. Exactly 8: does not.
    t.put("B1", 1.0 + 7.0 * ULP1);
    t.put("B2", 1.0 + 8.0 * ULP1);
    t.set("C1", "=A1-B1");
    t.set("C2", "=A1-B2");
    assert_eq!(t.get("C1"), Value::Number(0.0));
    assert_eq!(t.get("C2"), Value::Number(-8.0 * ULP1));
    // The binade is the LEFT operand's (Excel observed, request 4, excel-probe-4.00044 to 00046).
    t.put("A3", 0.4999999999999998);
    t.put("B3", 0.5000000000000002);
    t.set("C3", "=A3-B3");
    t.set("C4", "=B3-A3");
    assert_eq!(t.get("C3"), Value::Number(-4.440892098500626e-16));
    assert_eq!(t.get("C4"), Value::Number(0.0));
}

#[test]
fn the_snap_does_not_apply_inside_parentheses_unary_if_or_inner_steps() {
    let mut t = T::new();
    t.put("A1", 1.0);
    t.put("B1", 1.0 + ULP1);
    let residue = 1.0 - (1.0 + ULP1);
    for (at, f) in [
        ("C1", "=(A1-B1)"),
        ("C2", "=+(A1-B1)"),
        ("C3", "=-(B1-A1)"),
        ("C4", "=IF(TRUE,A1-B1)"),
        ("C5", "=A1-B1+0"),       // the root is the +0; the inner subtraction is not a root
        ("C6", "=SUM(A1,-B1,0)"), // the last addition is residue+0
    ] {
        t.set(at, f);
        assert_eq!(t.get(at), Value::Number(residue), "{f}");
    }
    // Excel observed (request 3): =SUM(A-B) does not snap (the subtraction is an inner step, the SUM's
    // last addition is residue + 0), and neither does a root that adds a zero.
    t.set("C7", "=SUM(A1-B1)");
    t.set("C8", "=A1-B1-0");
    assert_eq!(t.get("C7"), Value::Number(residue));
    assert_eq!(t.get("C8"), Value::Number(residue));
    // Only the LAST addition of a SUM snaps: 3+1e16-1e16 is 4, not 0.
    t.put("D1", 3.0);
    t.put("D2", 1e16);
    t.put("D3", -1e16);
    t.put("D4", 0.0);
    t.set("E1", "=SUM(D1:D4)");
    assert_eq!(t.get("E1"), Value::Number(4.0));
}

#[test]
fn same_sign_and_exact_arithmetic_never_snap() {
    let mut t = T::new();
    t.put("A1", 1e-20);
    t.put("B1", 2e-20);
    t.set("C1", "=A1+B1"); // a tiny sum of small numbers is not a cancellation
    t.set("C2", "=A1-B1");
    t.put("A2", 3.0);
    t.put("B2", 2.0);
    t.set("C3", "=A2-B2");
    assert_eq!(t.num("C1"), 1e-20 + 2e-20);
    assert_eq!(t.num("C2"), 1e-20 - 2e-20);
    assert_eq!(t.num("C3"), 1.0);
}

#[test]
fn the_snap_threshold_follows_the_left_operand_across_a_binade_boundary() {
    // Windows Excel 16.0.20430, snap probe rows 226, 231 and 236 (=A-B, A just below 1.0): the
    // threshold is 2^-50 (A's binade), not 2^-49 (B's), so 7.8e-16 snaps and 9.99e-16 does not.
    let mut t = T::new();
    t.put("A1", 0.9999999999999999);
    t.put("B1", 1.0000000000000004);
    t.put("B2", 1.0000000000000007);
    t.put("B3", 1.0000000000000009);
    t.set("D1", "=A1-B1");
    t.set("D2", "=A1-B2");
    t.set("D3", "=A1-B3");
    assert_eq!(t.get("D1"), Value::Number(0.0));
    assert_eq!(t.get("D2"), Value::Number(0.0));
    assert_eq!(t.get("D3"), Value::Number(-9.992007221626409e-16));
}

#[test]
fn the_snap_leaves_error_and_text_operands_alone() {
    let mut t = T::new();
    t.put("B1", 1.0 + ULP1);
    t.set("A1", "=1/0");
    t.set("C1", "=A1-B1");
    assert_eq!(t.get("C1"), Value::Error(CellError::Div0));
    t.set("A2", "abc");
    t.set("C2", "=A2-B1");
    assert_eq!(t.get("C2"), Value::Error(CellError::Value));
    t.set("A3", "1"); // numeric text still converts, then the subtraction snaps like any other
    t.put("B3", 1.0);
    t.set("C3", "=A3-B3");
    assert_eq!(t.get("C3"), Value::Number(0.0));
}

// ---- the snap in a defined name's body (Windows Excel 16.0.20430, name probe: 8 bodies x 12 positions) ----

impl T {
    fn name(&mut self, name: &str, body: &str) {
        self.wb.names.push(gridcraft_model::DefinedName {
            name: name.into(),
            scope: None,
            formula: body.into(),
            comment: String::new(),
            hidden: false,
        });
    }
}

#[test]
fn a_defined_name_is_a_formula_of_its_own_so_its_body_snaps_wherever_it_is_used() {
    let mut t = T::new();
    t.put("A1", 1.0);
    t.put("B1", 1.0 + ULP1);
    // (body, does Excel snap it?) Only the shape of the BODY matters, not where the name is used.
    let bodies = [
        ("N_sub", "Sheet1!$A$1-Sheet1!$B$1", true),
        ("N_par", "(Sheet1!$A$1-Sheet1!$B$1)", false),
        ("N_plus0", "Sheet1!$A$1-Sheet1!$B$1+0", false),
        ("N_sum", "SUM(Sheet1!$A$1,-Sheet1!$B$1)", true),
        ("N_neg", "-(Sheet1!$B$1-Sheet1!$A$1)", false),
        ("N_ref", "N_sub", true), // a name that is a name inherits it
        ("N_if", "IF(TRUE,Sheet1!$A$1-Sheet1!$B$1)", false),
        ("N_sumsub", "SUM(Sheet1!$A$1-Sheet1!$B$1)", false),
    ];
    for (name, body, _) in bodies {
        t.name(name, body);
    }
    // All 12 positions of the Excel probe.
    let uses = ["{n}", "{n}*1", "{n}+0", "0+{n}", "({n})", "-{n}", "+{n}", "SUM({n})", "IF(TRUE,{n})", "ABS({n})", "1*{n}", "{n}-0"];
    let mut row = 2;
    for (name, body, snaps) in bodies {
        for u in uses {
            let at = format!("D{row}");
            t.set(&at, &format!("={}", u.replace("{n}", name)));
            // Every body is -1 ulp (A1-B1) or exactly 0; a minus sign or ABS flips the residue.
            let flips = u.starts_with('-') || u.starts_with("ABS");
            let want = if snaps {
                0.0
            } else if flips {
                ULP1
            } else {
                -ULP1
            };
            assert_eq!(t.get(&at), Value::Number(want), "{u} over {name} = {body}");
            row += 1;
        }
    }
}

#[test]
fn excels_own_gap_shapes_from_lookup_shapes_xlsx() {
    // excel-evidence/request-3/files/lookup-shapes.xlsx, Sheet1!H15:H26: the gaps of 1, 4 and 7 ulps
    // snap to 0 as =Gap, =Gap*1 and =Gap+0; the gap of 8 ulps keeps its residue.
    let mut t = T::new();
    for (cell, v) in [("A1", 1.0000000000000002), ("A2", 1.0000000000000009), ("A3", 1.0000000000000016), ("A4", 1.0000000000000018)] {
        t.put(cell, v);
    }
    for (cell, v) in [("B1", 1.0), ("B2", 1.0), ("B3", 1.0), ("B4", 1.0)] {
        t.put(cell, v);
    }
    for (name, row) in [("Gap_1", 1), ("Gap_4", 2), ("Gap_7", 3), ("Gap_8", 4)] {
        t.name(name, &format!("Sheet1!$A${row}-Sheet1!$B${row}"));
    }
    let mut r = 10;
    for (name, residue) in [("Gap_1", 0.0), ("Gap_4", 0.0), ("Gap_7", 0.0), ("Gap_8", 1.7763568394002505e-15)] {
        for f in ["{n}", "{n}*1", "{n}+0"] {
            let at = format!("F{r}");
            t.set(&at, &format!("={}", f.replace("{n}", name)));
            assert_eq!(t.get(&at), Value::Number(residue), "{f} over {name}");
            r += 1;
        }
    }
}

#[test]
fn a_name_that_refers_to_a_range_still_refers_to_it() {
    // Only a reference answers these: ISREF, ROW and OFFSET over a name. Wrapping every name
    // body in value_root would turn the range into an array and fail all three.
    let mut t = T::new();
    t.put("A1", 1.0);
    t.put("A2", 2.0);
    t.put("B1", 3.0);
    t.put("B2", 4.0);
    t.name("Block", "Sheet1!$A$1:$B$2");
    t.set("D1", "=ISREF(Block)");
    t.set("D2", "=MIN(ROW(Block))");
    t.set("D3", "=SUM(OFFSET(Block,0,1))");
    assert_eq!(t.get("D1"), Value::Bool(true));
    assert_eq!(t.num("D2"), 1.0);
    assert_eq!(t.num("D3"), 7.0);
}

#[test]
fn a_name_whose_body_is_an_array_difference_snaps_element_wise() {
    // Windows Excel (review probe): INDEX(Arr,1)*1 and INDEX(Arr,2)*1 are 0 for Arr = A1:A2-B1:B2.
    let mut t = T::new();
    for (a, b) in [("A1", "B1"), ("A2", "B2")] {
        t.put(a, 1.0);
        t.put(b, 1.0 + ULP1);
    }
    t.name("Arr", "Sheet1!$A$1:$A$2-Sheet1!$B$1:$B$2");
    t.set("D1", "=INDEX(Arr,1)*1");
    t.set("D2", "=INDEX(Arr,2)*1");
    t.set("D3", "=SUM(Arr)");
    assert_eq!((t.get("D1"), t.get("D2")), (Value::Number(0.0), Value::Number(0.0)));
    assert_eq!(t.get("D3"), Value::Number(0.0));
}

#[test]
fn a_sheet_scoped_name_snaps_on_its_own_sheet() {
    // Windows Excel (review probe): a sheet-scoped Loc = S2!$A$1-S2!$B$1 gives 0 as =S2!Loc*1.
    let mut t = T::new();
    t.wb.sheets.push(std::sync::Arc::new(gridcraft_model::Sheet::new("S2")));
    t.wb.sheet_mut(1).unwrap().cells.set(c("A1"), Cell::value(Value::Number(1.0)));
    t.wb.sheet_mut(1).unwrap().cells.set(c("B1"), Cell::value(Value::Number(1.0 + ULP1)));
    t.wb.names.push(gridcraft_model::DefinedName {
        name: "Loc".into(),
        scope: Some(1),
        formula: "S2!$A$1-S2!$B$1".into(),
        comment: String::new(),
        hidden: false,
    });
    t.set("D1", "=S2!Loc*1");
    t.set("D2", "=S2!Loc");
    assert_eq!((t.get("D1"), t.get("D2")), (Value::Number(0.0), Value::Number(0.0)));
}

#[test]
fn recursive_names_stay_bounded_and_cost_the_same_depth_as_before_the_snap() {
    let mut t = T::new();
    // A name that refers to itself ends in #CALC!, not a stack overflow.
    t.name("Rec", "Rec-1");
    t.set("A1", "=Rec");
    assert_eq!(t.get("A1"), Value::Error(CellError::Calc));
    // A long chain of names, each one `-0` over the last, runs out of depth at the same place as it did
    // before names were snapped (value_root must charge its own level, or each link would be cheaper
    // and a chain through cells could overflow the stack).
    t.name("Nm_0", "1");
    for i in 1..=250 {
        t.name(&format!("Nm_{i}"), &format!("Nm_{}-0", i - 1));
    }
    t.set("A2", "=Nm_250");
    assert_eq!(t.get("A2"), Value::Error(CellError::Calc));
    t.set("A3", "=Nm_20");
    assert_eq!(t.get("A3"), Value::Number(1.0));
}
