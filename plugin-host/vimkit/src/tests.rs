use super::*;
use listkit::styled::cells_text;

/// A fresh editor fed `keys` one after the other; the events in order.
fn run(text: &str, keys: &[&str]) -> (Editor, Vec<Event>) {
    let mut e = Editor::with_text(text);
    e.row = 0;
    e.col = 0;
    let evs = keys.iter().map(|k| e.key(k)).collect();
    (e, evs)
}

fn after(text: &str, keys: &[&str]) -> String {
    run(text, keys).0.text()
}

/// The typed characters of `s`, as single-key presses (a space as
/// "Space").
fn typed(s: &str) -> Vec<String> {
    s.chars().map(|c| if c == ' ' { "Space".to_string() } else { c.to_string() }).collect()
}

fn type_in(e: &mut Editor, s: &str) {
    for k in typed(s) {
        e.key(&k);
    }
}

#[test]
fn empty_editor() {
    let e = Editor::new();
    assert!(e.is_empty());
    assert_eq!(e.text(), "");
    assert_eq!(e.mode(), Mode::Normal);
    assert_eq!(e.cursor(), (0, 0));
    assert_eq!(e.status(), "");
}

#[test]
fn insert_then_normal_edit() {
    let mut e = Editor::new();
    assert_eq!(e.key("i"), Event::Redraw);
    assert_eq!(e.mode(), Mode::Insert);
    assert_eq!(e.status(), "-- INSERT --");
    type_in(&mut e, "hello");
    assert_eq!(e.key("Escape"), Event::Redraw);
    assert_eq!(e.mode(), Mode::Normal);
    // Escape steps the cursor back onto the last character.
    assert_eq!(e.cursor(), (0, 4));
    e.key("0");
    e.key("x");
    assert_eq!(e.text(), "ello");
    assert_eq!(e.cursor(), (0, 0));
}

#[test]
fn enter_submits_escape_leaves() {
    let mut e = Editor::new();
    e.key("i");
    type_in(&mut e, "hi");
    assert_eq!(e.key("Enter"), Event::Submit);
    assert_eq!(e.text(), "hi", "Submit leaves the text to the owner");
    assert_eq!(e.mode(), Mode::Insert);
    e.key("Escape");
    assert_eq!(e.key("Enter"), Event::Submit);
    assert_eq!(e.key("Escape"), Event::Leave);
    // A pending command: Escape only cancels it.
    e.key("d");
    assert_eq!(e.status(), "d");
    assert_eq!(e.key("Escape"), Event::Redraw);
    assert_eq!(e.status(), "");
    assert_eq!(e.key("Escape"), Event::Leave);
}

#[test]
fn set_text_and_clear() {
    let mut e = Editor::new();
    e.insert();
    e.set_text("abc\ndef");
    assert_eq!(e.mode(), Mode::Insert);
    assert_eq!(e.cursor(), (1, 3), "the cursor is after the text");
    type_in(&mut e, "!");
    assert_eq!(e.text(), "abc\ndef!");
    e.clear();
    assert!(e.is_empty());
    assert_eq!(e.cursor(), (0, 0));
    assert_eq!(e.mode(), Mode::Insert);
    let e = Editor::with_text("xyz");
    assert_eq!(e.cursor(), (0, 2), "normal mode sits on the last character");
}

#[test]
fn hjkl_and_arrows_with_counts() {
    let (mut e, _) = run("abcdef\nxy\nlonger line", &["l", "l", "l"]);
    assert_eq!(e.cursor(), (0, 3));
    e.key("j");
    assert_eq!(e.cursor(), (1, 1), "a short line clamps");
    e.key("j");
    assert_eq!(e.cursor(), (2, 3), "the wanted column comes back");
    e.key("2");
    e.key("k");
    assert_eq!(e.cursor(), (0, 3));
    e.key("Down");
    e.key("Right");
    assert_eq!(e.cursor(), (1, 1));
    e.key("Left");
    e.key("Up");
    assert_eq!(e.cursor(), (0, 0));
    e.key("1");
    e.key("0");
    e.key("l");
    assert_eq!(e.cursor(), (0, 5), "10l stops at the end");
}

#[test]
fn line_motions() {
    let (mut e, _) = run("   indented text", &["$"]);
    assert_eq!(e.cursor(), (0, 15));
    e.key("0");
    assert_eq!(e.cursor(), (0, 0));
    e.key("^");
    assert_eq!(e.cursor(), (0, 3));
    e.key("End");
    assert_eq!(e.cursor(), (0, 15));
    e.key("Home");
    assert_eq!(e.cursor(), (0, 0));
}

#[test]
fn word_motions() {
    let (mut e, _) = run("foo bar-baz  qux\n\nnext", &["w"]);
    assert_eq!(e.cursor(), (0, 4));
    e.key("w");
    assert_eq!(e.cursor(), (0, 7), "punctuation is its own word");
    e.key("w");
    assert_eq!(e.cursor(), (0, 8));
    e.key("w");
    assert_eq!(e.cursor(), (0, 13));
    e.key("w");
    assert_eq!(e.cursor(), (1, 0), "an empty line is a word");
    e.key("w");
    assert_eq!(e.cursor(), (2, 0));
    e.key("w");
    assert_eq!(e.cursor(), (2, 3), "the end of the buffer");
    e.key("b");
    assert_eq!(e.cursor(), (2, 0));
    e.key("b");
    assert_eq!(e.cursor(), (1, 0));
    e.key("b");
    assert_eq!(e.cursor(), (0, 13));
    e.key("3");
    e.key("b");
    assert_eq!(e.cursor(), (0, 4));
    e.key("e");
    assert_eq!(e.cursor(), (0, 6));
    e.key("e");
    assert_eq!(e.cursor(), (0, 7));
    e.key("W");
    assert_eq!(e.cursor(), (0, 13), "W skips punctuation");
    e.key("0");
    e.key("E");
    assert_eq!(e.cursor(), (0, 2));
    e.key("E");
    assert_eq!(e.cursor(), (0, 10));
    e.key("B");
    assert_eq!(e.cursor(), (0, 4));
}

#[test]
fn count_3w() {
    let (e, _) = run("one two three four", &["3", "w"]);
    assert_eq!(e.cursor(), (0, 14));
}

#[test]
fn paragraph_motions() {
    let text = "a\nb\n\nc\nd\n\n\ne";
    let (mut e, _) = run(text, &["}"]);
    assert_eq!(e.cursor(), (2, 0));
    e.key("}");
    assert_eq!(e.cursor(), (5, 0));
    e.key("}");
    assert_eq!(e.cursor(), (7, 0), "the last line when no blank follows");
    e.key("{");
    assert_eq!(e.cursor(), (6, 0));
    e.key("{");
    assert_eq!(e.cursor(), (2, 0));
    e.key("{");
    assert_eq!(e.cursor(), (0, 0));
    // d} deletes to the blank line, leaving it.
    assert_eq!(after(text, &["d", "}"]), "\nc\nd\n\n\ne");
}

#[test]
fn gg_and_g() {
    let (mut e, _) = run("1\n2\n  3\n4", &["G"]);
    assert_eq!(e.cursor(), (3, 0));
    e.key("g");
    assert_eq!(e.status(), "g");
    e.key("g");
    assert_eq!(e.cursor(), (0, 0));
    e.key("3");
    e.key("G");
    assert_eq!(e.cursor(), (2, 2), "3G is line 3, first non-blank");
    e.key("2");
    e.key("g");
    e.key("g");
    assert_eq!(e.cursor(), (1, 0));
}

#[test]
fn x_dd_dw_and_counts() {
    assert_eq!(after("hello", &["x"]), "ello");
    assert_eq!(after("hello", &["3", "x"]), "lo");
    assert_eq!(after("hello", &["$", "x"]), "hell");
    assert_eq!(after("ab", &["l", "X"]), "b");
    assert_eq!(after("a\nb\nc", &["d", "d"]), "b\nc");
    assert_eq!(after("a\nb\nc", &["2", "d", "d"]), "c");
    assert_eq!(after("a\nb\nc", &["d", "2", "d"]), "c");
    assert_eq!(after("a\nb\nc", &["j", "d", "j"]), "a");
    assert_eq!(after("one two three", &["d", "w"]), "two three");
    assert_eq!(after("one two three", &["2", "d", "w"]), "three");
    assert_eq!(after("one two three", &["d", "2", "w"]), "three");
    assert_eq!(after("a b c", &["d", "$"]), "");
    assert_eq!(after("a b c", &["w", "D"]), "a ");
    assert_eq!(after("abc", &["d", "l"]), "bc");
    assert_eq!(after("abc", &["$", "d", "l"]), "ab");
    assert_eq!(after("abc", &["$", "d", "h"]), "ac");
    assert_eq!(after("abc def", &["$", "d", "b"]), "abc f");
    assert_eq!(after("abc def", &["d", "e"]), " def");
    assert_eq!(after("1\n2\n3", &["j", "d", "G"]), "1");
    assert_eq!(after("1\n2\n3", &["j", "d", "g", "g"]), "3");
}

#[test]
fn dd_on_the_last_line() {
    let (e, _) = run("a\nb\nc", &["G", "d", "d"]);
    assert_eq!(e.text(), "a\nb");
    assert_eq!(e.cursor(), (1, 0), "the cursor moves up onto the new last line");
    let (e, _) = run("only", &["d", "d"]);
    assert!(e.is_empty());
    assert_eq!(e.cursor(), (0, 0));
}

#[test]
fn dw_on_the_last_word_keeps_the_newline() {
    assert_eq!(after("one two\nthree", &["w", "d", "w"]), "one \nthree");
    assert_eq!(after("one\n\ntwo", &["d", "w"]), "\n\ntwo");
}

#[test]
fn cw_changes_the_word_and_keeps_the_blank() {
    let mut e = Editor::with_text("one two three");
    e.row = 0;
    e.col = 0;
    e.key("c");
    e.key("w");
    assert_eq!(e.mode(), Mode::Insert);
    assert_eq!(e.text(), " two three");
    type_in(&mut e, "ONE");
    e.key("Escape");
    assert_eq!(e.text(), "ONE two three");
    assert_eq!(after("a  b", &["l", "c", "w", "X", "Escape"]), "aXb", "cw on a blank eats the blanks");
}

#[test]
fn c_and_s_variants() {
    let (mut e, _) = run("hello world", &["w", "C"]);
    assert_eq!(e.text(), "hello ");
    assert_eq!(e.mode(), Mode::Insert);
    assert_eq!(e.cursor(), (0, 6));
    type_in(&mut e, "there");
    e.key("Escape");
    assert_eq!(e.text(), "hello there");
    assert_eq!(after("abc", &["s", "X", "Escape"]), "Xbc");
    assert_eq!(after("abc", &["$", "s", "X", "Escape"]), "abX");
    assert_eq!(after("abc\ndef", &["S", "X", "Escape"]), "X\ndef");
    assert_eq!(after("abc\ndef", &["c", "c", "X", "Escape"]), "X\ndef");
    assert_eq!(after("abc\ndef", &["2", "c", "c", "X", "Escape"]), "X");
    // C on an empty line still enters insert mode.
    let (e, _) = run("", &["C"]);
    assert_eq!(e.mode(), Mode::Insert);
}

#[test]
fn yank_and_put() {
    let (mut e, evs) = run("one\ntwo", &["y", "y"]);
    assert_eq!(evs[1], Event::Yank("one\n".into()));
    e.key("p");
    assert_eq!(e.text(), "one\none\ntwo");
    assert_eq!(e.cursor(), (1, 0));
    e.key("P");
    assert_eq!(e.text(), "one\none\none\ntwo");
    let (mut e, evs) = run("one two", &["y", "w"]);
    assert_eq!(evs[1], Event::Yank("one ".into()));
    assert_eq!(e.cursor(), (0, 0), "y leaves the cursor at the start");
    e.key("$");
    e.key("p");
    assert_eq!(e.text(), "one twoone ");
    assert_eq!(e.cursor(), (0, 10));
    e.key("0");
    e.key("P");
    assert_eq!(e.text(), "one one twoone ");
    assert_eq!(e.cursor(), (0, 3));
    // A deleted word is in the register too, and counts repeat.
    let (mut e, _) = run("ab", &["x"]);
    e.key("3");
    e.key("p");
    assert_eq!(e.text(), "baaa");
    assert_eq!(e.cursor(), (0, 3));
    // Pasting into an empty document.
    let (mut e, _) = run("abc", &["y", "y", "d", "d"]);
    e.key("p");
    assert_eq!(e.text(), "\nabc");
}

#[test]
fn put_charwise_text_with_newlines() {
    let (mut e, evs) = run("ab\ncd", &["l", "v", "j", "y"]);
    assert_eq!(evs[3], Event::Yank("b\ncd".into()));
    e.key("G");
    e.key("$");
    e.key("p");
    assert_eq!(e.text(), "ab\ncdb\ncd");
    assert_eq!(e.cursor(), (2, 1));
}

#[test]
fn o_and_big_o() {
    let mut e = Editor::with_text("a\nb");
    e.row = 0;
    e.col = 0;
    e.key("o");
    assert_eq!(e.mode(), Mode::Insert);
    type_in(&mut e, "new");
    e.key("Escape");
    assert_eq!(e.text(), "a\nnew\nb");
    assert_eq!(e.cursor(), (1, 2));
    e.key("O");
    type_in(&mut e, "x");
    e.key("Escape");
    assert_eq!(e.text(), "a\nx\nnew\nb");
}

#[test]
fn a_big_a_and_big_i() {
    assert_eq!(after("ab", &["a", "X", "Escape"]), "aXb");
    assert_eq!(after("ab", &["A", "X", "Escape"]), "abX");
    assert_eq!(after("  ab", &["$", "I", "X", "Escape"]), "  Xab");
    assert_eq!(after("", &["a", "X", "Escape"]), "X", "a on an empty line");
    assert_eq!(after("ab", &["i", "X", "Escape"]), "Xab");
}

#[test]
fn r_and_tilde() {
    assert_eq!(after("abc", &["r", "X"]), "Xbc");
    assert_eq!(after("abc", &["2", "r", "X"]), "XXc");
    assert_eq!(after("abc", &["5", "r", "X"]), "abc", "too few characters: nothing");
    assert_eq!(after("ab c", &["3", "~"]), "AB c");
    let (e, _) = run("abc", &["r", "Escape"]);
    assert_eq!(e.text(), "abc");
    assert_eq!(e.status(), "", "Escape cancels r");
}

#[test]
fn j_joins() {
    let (e, _) = run("one\n   two\nthree", &["J"]);
    assert_eq!(e.text(), "one two\nthree");
    assert_eq!(e.cursor(), (0, 3));
    assert_eq!(after("a\nb\nc\nd", &["3", "J"]), "a b c\nd");
    assert_eq!(after("a \nb", &["J"]), "a b", "a trailing blank is not doubled");
    assert_eq!(after("a\n\nb", &["J"]), "a\nb", "an empty line adds no blank");
    assert_eq!(after("a", &["J"]), "a");
}

#[test]
fn undo_and_redo() {
    let mut e = Editor::new();
    e.key("i");
    type_in(&mut e, "hello world");
    e.key("Escape");
    e.key("b");
    e.key("d");
    e.key("w");
    assert_eq!(e.text(), "hello ");
    assert_eq!(e.key("u"), Event::Redraw);
    assert_eq!(e.text(), "hello world");
    assert_eq!(e.cursor(), (0, 6), "the cursor returns to where the change was made");
    e.key("u");
    assert_eq!(e.text(), "", "the whole insert session is one step");
    assert_eq!(e.key("u"), Event::Redraw, "nothing left: the key is still handled");
    assert_eq!(e.key("C-r"), Event::Redraw);
    assert_eq!(e.text(), "hello world");
    e.key("C-r");
    assert_eq!(e.text(), "hello ");
    assert_eq!(e.key("C-r"), Event::None);
    // A new change drops the redo history.
    e.key("x");
    assert_eq!(e.key("C-r"), Event::None);
    // An insert that typed nothing leaves no undo step.
    let (mut e, _) = run("abc", &["x", "i", "Escape", "u"]);
    assert_eq!(e.text(), "abc");
    e.key("2");
    e.key("u");
    assert_eq!(e.text(), "abc");
}

#[test]
fn visual_yank_delete_change() {
    let (e, evs) = run("hello world", &["v", "e", "y"]);
    assert_eq!(e.status(), "");
    assert_eq!(evs[0], Event::Redraw);
    assert_eq!(evs[2], Event::Yank("hello".into()));
    assert_eq!(e.mode(), Mode::Normal);
    assert_eq!(e.cursor(), (0, 0));
    let (e, _) = run("hello world", &["v", "e", "d"]);
    assert_eq!(e.text(), " world");
    let (e, _) = run("hello world", &["w", "v", "$", "x"]);
    assert_eq!(e.text(), "hello ");
    let (mut e, _) = run("hello world", &["v", "e", "c"]);
    assert_eq!(e.mode(), Mode::Insert);
    type_in(&mut e, "bye");
    e.key("Escape");
    assert_eq!(e.text(), "bye world");
    // Across lines, and backwards from the anchor.
    assert_eq!(after("ab\ncd\nef", &["l", "v", "j", "d"]), "a\nef");
    assert_eq!(after("ab\ncd\nef", &["l", "v", "j", "h", "d"]), "ad\nef");
    assert_eq!(after("abc", &["$", "v", "h", "d"]), "a");
    // Escape leaves visual mode with the text intact.
    let (e, _) = run("abc", &["v", "l", "Escape"]);
    assert_eq!(e.mode(), Mode::Normal);
    assert_eq!(e.text(), "abc");
    // o swaps the ends.
    let (e, _) = run("abcd", &["l", "v", "l", "o"]);
    assert_eq!(e.cursor(), (0, 1));
    assert_eq!(e.anchor, (0, 2));
}

#[test]
fn visual_mode_status_and_toggles() {
    let mut e = Editor::with_text("abc\ndef");
    e.key("v");
    assert_eq!(e.status(), "-- VISUAL --");
    e.key("V");
    assert_eq!(e.status(), "-- VISUAL LINE --");
    e.key("C-v");
    assert_eq!(e.status(), "-- VISUAL BLOCK --");
    e.key("C-v");
    assert_eq!(e.mode(), Mode::Normal);
    e.key("v");
    e.key("v");
    assert_eq!(e.mode(), Mode::Normal, "v again leaves");
    assert_eq!(e.key("Enter"), Event::Submit, "Enter in normal mode submits");
}

#[test]
fn visual_line() {
    let (mut e, evs) = run("a\nb\nc", &["V", "j", "y"]);
    assert_eq!(evs[2], Event::Yank("a\nb\n".into()));
    e.key("G");
    e.key("p");
    assert_eq!(e.text(), "a\nb\nc\na\nb");
    assert_eq!(after("a\nb\nc", &["j", "V", "d"]), "a\nc");
    assert_eq!(after("a\nb\nc", &["V", "j", "j", "d"]), "");
    let (mut e, _) = run("a\nb\nc", &["V", "j", "c"]);
    assert_eq!(e.mode(), Mode::Insert);
    type_in(&mut e, "x");
    e.key("Escape");
    assert_eq!(e.text(), "x\nc");
}

#[test]
fn visual_block_yank_delete() {
    let text = "abcd\nefgh\nijkl";
    let (e, evs) = run(text, &["l", "C-v", "j", "j", "l", "y"]);
    assert_eq!(evs[5], Event::Yank("bc\nfg\njk".into()));
    assert_eq!(e.cursor(), (0, 1));
    let (e, _) = run(text, &["l", "C-v", "j", "j", "l", "d"]);
    assert_eq!(e.text(), "ad\neh\nil");
    assert_eq!(e.cursor(), (0, 1));
    // Short lines take what they have.
    assert_eq!(after("abcd\nef\nijkl", &["l", "l", "C-v", "j", "j", "l", "d"]), "ab\nef\nij");
    // A block paste puts the columns back.
    let (mut e, _) = run(text, &["l", "C-v", "j", "j", "l", "d", "$", "p"]);
    assert_eq!(e.text(), "adbc\nehfg\niljk");
    assert_eq!(e.cursor(), (0, 2));
    e.key("u");
    assert_eq!(e.text(), "ad\neh\nil");
}

#[test]
fn visual_block_column_insert() {
    let text = "one\ntwo\nthree";
    assert_eq!(after(text, &["C-v", "j", "j", "I", "#", "Escape"]), "#one\n#two\n#three");
    assert_eq!(after(text, &["C-v", "j", "j", "I", "#", "Space", "Escape"]), "# one\n# two\n# three");
    // A: after the block's right edge; short lines are padded.
    assert_eq!(after("ab\nabcd\nabc", &["C-v", "j", "j", "l", "l", "A", "!", "Escape"]), "ab !\nabc!d\nabc!");
    // I skips lines shorter than the column.
    assert_eq!(after("abc\na\nabc", &["l", "C-v", "j", "j", "I", "X", "Escape"]), "aXbc\na\naXbc");
    // c: the block replaced on every line.
    assert_eq!(after("abc\nabc\nabc", &["l", "C-v", "j", "j", "c", "Z", "Z", "Escape"]), "aZZc\naZZc\naZZc");
    // Backspace during the insert edits what gets repeated.
    assert_eq!(after("a\nb", &["C-v", "j", "I", "x", "y", "BSpace", "Escape"]), "xa\nxb");
    // An arrow key gives up the repetition; only the first line changes.
    assert_eq!(after("a\nb", &["C-v", "j", "I", "x", "Left", "Escape"]), "xa\nb");
    // One undo step for the whole thing.
    let (mut e, _) = run(text, &["C-v", "j", "j", "I", "#", "Escape"]);
    e.key("u");
    assert_eq!(e.text(), text);
}

#[test]
fn visual_r_replaces_the_selection() {
    assert_eq!(after("abcd", &["l", "v", "l", "r", "x"]), "axxd");
    assert_eq!(after("ab\ncd", &["V", "j", "r", "-"]), "--\n--");
    assert_eq!(after("abc\nabc", &["l", "C-v", "j", "r", "."]), "a.c\na.c");
}

#[test]
fn visual_p_replaces_with_the_register() {
    let (e, _) = run("one two", &["y", "e", "w", "v", "e", "p"]);
    assert_eq!(e.text(), "one one");
    let (e, _) = run("one two", &["w", "y", "e", "0", "v", "e", "p"]);
    assert_eq!(e.text(), "two two");
}

#[test]
fn search_forward_backward_and_wrap() {
    let text = "alpha beta\ngamma beta\nbeta";
    let mut e = Editor::with_text(text);
    e.row = 0;
    e.col = 0;
    e.key("/");
    assert_eq!(e.mode(), Mode::Search);
    assert_eq!(e.status(), "/");
    type_in(&mut e, "beta");
    assert_eq!(e.status(), "/beta");
    assert_eq!(e.key("Enter"), Event::Redraw);
    assert_eq!(e.mode(), Mode::Normal);
    assert_eq!(e.cursor(), (0, 6));
    e.key("n");
    assert_eq!(e.cursor(), (1, 6));
    e.key("n");
    assert_eq!(e.cursor(), (2, 0));
    e.key("n");
    assert_eq!(e.cursor(), (0, 6), "wraps to the top");
    e.key("N");
    assert_eq!(e.cursor(), (2, 0), "N goes the other way and wraps");
    e.key("2");
    e.key("N");
    assert_eq!(e.cursor(), (0, 6));
    // ? searches backwards; a capital makes it case-sensitive.
    e.key("?");
    type_in(&mut e, "gamma");
    e.key("Enter");
    assert_eq!(e.cursor(), (1, 0));
    e.key("/");
    type_in(&mut e, "ALPHA");
    e.key("Enter");
    assert_eq!(e.cursor(), (1, 0), "no match: the cursor stays");
    e.key("/");
    type_in(&mut e, "alpha");
    e.key("Enter");
    assert_eq!(e.cursor(), (0, 0));
    // Escape cancels the search; BSpace on an empty pattern too.
    e.key("/");
    type_in(&mut e, "x");
    e.key("Escape");
    assert_eq!(e.mode(), Mode::Normal);
    e.key("/");
    e.key("BSpace");
    assert_eq!(e.mode(), Mode::Normal);
    // An empty pattern repeats the last search.
    e.key("/");
    e.key("Enter");
    assert_eq!(e.cursor(), (0, 0), "alpha only occurs once");
    e.key("n");
    assert_eq!(e.cursor(), (0, 0));
}

#[test]
fn search_as_a_match_within_the_line() {
    let (e, _) = run("xx ab xx ab", &["/", "a", "b", "Enter", "n"]);
    assert_eq!(e.cursor(), (0, 9));
}

#[test]
fn find_motions() {
    let (mut e, _) = run("a(b)c(d)", &["f", "("]);
    assert_eq!(e.cursor(), (0, 1));
    e.key("2");
    e.key("f");
    e.key(")");
    assert_eq!(e.cursor(), (0, 7));
    e.key("F");
    e.key("b");
    assert_eq!(e.cursor(), (0, 2));
    e.key("t");
    e.key("c");
    assert_eq!(e.cursor(), (0, 3));
    e.key("T");
    e.key("a");
    assert_eq!(e.cursor(), (0, 1));
    assert_eq!(after("a(b)c", &["d", "t", ")"]), ")c");
    assert_eq!(after("a(b)c", &["d", "f", ")"]), "c");
    assert_eq!(after("a(b)c", &["f", "z"]), "a(b)c");
}

#[test]
fn insert_mode_editing_keys() {
    let mut e = Editor::new();
    e.key("i");
    type_in(&mut e, "hello big world");
    e.key("BSpace");
    assert_eq!(e.text(), "hello big worl");
    e.key("C-w");
    assert_eq!(e.text(), "hello big ");
    e.key("C-w");
    assert_eq!(e.text(), "hello ");
    type_in(&mut e, "x");
    e.key("Home");
    assert_eq!(e.cursor(), (0, 0));
    e.key("DC");
    assert_eq!(e.text(), "ello x");
    e.key("End");
    assert_eq!(e.cursor(), (0, 6));
    e.key("Left");
    e.key("Left");
    type_in(&mut e, "+");
    assert_eq!(e.text(), "ello+ x");
    e.key("Right");
    assert_eq!(e.cursor(), (0, 6));
    e.key("C-u");
    assert_eq!(e.text(), "x");
    assert_eq!(e.key("C-h"), Event::None, "nothing before the cursor");
    e.key("End");
    e.key("C-h");
    assert_eq!(e.key("C-h"), Event::None, "nothing left to delete");
    assert_eq!(e.text(), "");
    e.key("Tab");
    assert_eq!(e.text(), "\t");
}

#[test]
fn insert_mode_newline_and_vertical_moves() {
    let mut e = Editor::new();
    e.key("i");
    type_in(&mut e, "first");
    e.key("C-j");
    type_in(&mut e, "second line");
    assert_eq!(e.text(), "first\nsecond line");
    assert_eq!(e.cursor(), (1, 11));
    e.key("Up");
    assert_eq!(e.cursor(), (0, 5), "clamped to the shorter line");
    e.key("Down");
    assert_eq!(e.cursor(), (1, 11), "the wanted column survives");
    e.key("Up");
    e.key("BSpace");
    e.key("BSpace");
    assert_eq!(e.text(), "fir\nsecond line");
    e.key("Down");
    e.key("Home");
    e.key("BSpace");
    assert_eq!(e.text(), "firsecond line", "BSpace at column 0 joins");
    e.key("C-j");
    e.key("Left");
    assert_eq!(e.cursor(), (0, 3));
    e.key("DC");
    assert_eq!(e.text(), "firsecond line", "DC at the end joins");
    e.key("Right");
    e.key("Right");
    assert_eq!(e.cursor(), (0, 5));
    // Escape at column 0 stays at column 0.
    e.key("Home");
    e.key("Escape");
    assert_eq!(e.cursor(), (0, 0));
}

#[test]
fn paste_keeps_newlines() {
    let mut e = Editor::new();
    e.key("i");
    type_in(&mut e, "ab");
    e.key("Left");
    e.paste("1\n2");
    assert_eq!(e.text(), "a1\n2b");
    assert_eq!(e.cursor(), (1, 1));
    assert_eq!(e.mode(), Mode::Insert);
    // In normal mode it goes after the cursor, like p, and undoes.
    let mut e = Editor::with_text("xy");
    e.row = 0;
    e.col = 0;
    e.paste("--\n--");
    assert_eq!(e.text(), "x--\n--y");
    assert_eq!(e.mode(), Mode::Normal);
    e.key("u");
    assert_eq!(e.text(), "xy");
    // Into an empty document.
    let mut e = Editor::new();
    e.paste("hi");
    assert_eq!(e.text(), "hi");
    assert_eq!(e.cursor(), (0, 1));
    // Into the search box, on one line.
    let mut e = Editor::with_text("a\nb");
    e.key("/");
    e.paste("b");
    assert_eq!(e.status(), "/b");
    e.key("Enter");
    assert_eq!(e.cursor(), (1, 0));
}

#[test]
fn space_and_tab_as_keys() {
    let mut e = Editor::new();
    e.key("i");
    e.key("a");
    e.key("Space");
    e.key("b");
    assert_eq!(e.text(), "a b");
    e.key("Escape");
    e.key("0");
    e.key("Space");
    assert_eq!(e.cursor(), (0, 1), "Space moves right in normal mode");
    e.key("BSpace");
    assert_eq!(e.cursor(), (0, 0), "BSpace moves left in normal mode");
    e.key("DC");
    assert_eq!(e.text(), " b", "DC deletes like x");
}

#[test]
fn unknown_keys_are_none() {
    let mut e = Editor::new();
    assert_eq!(e.key("F5"), Event::None);
    assert_eq!(e.key("M-x"), Event::None);
    assert_eq!(e.key("WheelUpPane"), Event::None);
    e.key("i");
    assert_eq!(e.key("F5"), Event::None);
    assert_eq!(e.key("C-t"), Event::None);
    assert_eq!(e.text(), "");
}

#[test]
fn invalid_command_is_dropped() {
    let (mut e, _) = run("abc", &["d", "q"]);
    assert_eq!(e.text(), "abc");
    assert_eq!(e.status(), "");
    e.key("x");
    assert_eq!(e.text(), "bc", "the next key starts fresh");
    let (e, _) = run("abc", &["3", "d", "Escape", "x"]);
    assert_eq!(e.text(), "bc");
}

#[test]
fn pending_shows_in_the_status() {
    let mut e = Editor::with_text("abc");
    e.key("3");
    assert_eq!(e.status(), "3");
    e.key("d");
    assert_eq!(e.status(), "3d");
    e.key("2");
    assert_eq!(e.status(), "3d2");
    e.key("w");
    assert_eq!(e.status(), "");
}

#[test]
fn render_cursor_and_padding() {
    let e = Editor::with_text("abc");
    let rows = e.render(10, 3);
    assert_eq!(rows.len(), 3);
    assert_eq!(cells_text(&rows[0]), "abc");
    assert_eq!(rows[0][2].1, ST_INVERT, "the cursor cell is inverted");
    assert_eq!(rows[0][0].1, 0);
    assert!(rows[1].is_empty() && rows[2].is_empty());
    // Insert mode past the end: a phantom cell carries the cursor.
    let mut e = Editor::with_text("abc");
    e.key("A");
    let rows = e.render(10, 1);
    assert_eq!(cells_text(&rows[0]), "abc ");
    assert_eq!(rows[0][3].1, ST_INVERT);
    // An empty document shows the cursor alone.
    let rows = Editor::new().render(5, 1);
    assert_eq!(rows[0], vec![(' ', ST_INVERT)]);
    assert!(Editor::new().render(5, 0).is_empty());
}

#[test]
fn render_soft_wraps_and_counts_rows() {
    let e = Editor::with_text("abcdefghij\nxy");
    assert_eq!(e.rows_needed(4), 4);
    assert_eq!(e.rows_needed(10), 2);
    assert_eq!(e.rows_needed(100), 2);
    let rows = e.render(4, 4);
    assert_eq!(cells_text(&rows[0]), "abcd");
    assert_eq!(cells_text(&rows[1]), "efgh");
    assert_eq!(cells_text(&rows[2]), "ij");
    assert_eq!(cells_text(&rows[3]), "xy");
    // A line that exactly fills the width, with the cursor after it in
    // insert mode, needs a row for the cursor.
    let mut e = Editor::with_text("abcd");
    assert_eq!(e.rows_needed(4), 1);
    e.key("A");
    assert_eq!(e.rows_needed(4), 2);
    // Wide glyphs take two cells and are not split.
    let e = Editor::with_text("a😀b");
    let rows = e.render(2, 3);
    assert_eq!(cells_text(&rows[0]), "a");
    assert_eq!(cells_text(&rows[1]), "😀");
    assert_eq!(cells_text(&rows[2]), "b");
}

#[test]
fn render_scrolls_to_keep_the_cursor_visible() {
    let mut e = Editor::with_text("1\n2\n3\n4\n5");
    e.row = 0;
    e.col = 0;
    let rows = e.render(10, 2);
    assert_eq!(cells_text(&rows[0]), "1");
    e.key("G");
    let rows = e.render(10, 2);
    assert_eq!(cells_text(&rows[0]), "4");
    assert_eq!(cells_text(&rows[1]), "5");
    e.key("k");
    let rows = e.render(10, 2);
    assert_eq!(cells_text(&rows[0]), "4", "the viewport stays while the cursor is in it");
    e.key("k");
    let rows = e.render(10, 2);
    assert_eq!(cells_text(&rows[0]), "3");
    // A shorter document than the viewport remembers: the top clamps.
    e.key("g");
    e.key("g");
    e.key("3");
    e.key("d");
    e.key("d");
    let rows = e.render(10, 2);
    assert_eq!(cells_text(&rows[0]), "4");
}

#[test]
fn render_marks_the_selection() {
    let (e, _) = run("abcd", &["l", "v", "l"]);
    let rows = e.render(10, 1);
    assert_eq!(rows[0][0].1, 0);
    assert_eq!(rows[0][1].1, ST_INVERT | ST_DIM);
    assert_eq!(rows[0][2].1, ST_INVERT, "the cursor is plainly inverted");
    assert_eq!(rows[0][3].1, 0);
    let (e, _) = run("ab\ncd\nef", &["j", "V"]);
    let rows = e.render(10, 3);
    assert!(rows[1].iter().all(|c| c.1 & ST_INVERT != 0));
    assert!(rows[0].iter().all(|c| c.1 == 0));
    assert!(rows[2].iter().all(|c| c.1 == 0));
    let (e, _) = run("abc\nabc", &["l", "C-v", "j"]);
    let rows = e.render(10, 2);
    assert_eq!(rows[0][1].1, ST_INVERT | ST_DIM);
    assert_eq!(rows[1][1].1, ST_INVERT);
    assert_eq!(rows[0][2].1, 0);
}

#[test]
fn counts_apply_to_insert_entry_and_paste() {
    // A count before i is ignored rather than repeated (not vim, kept small).
    assert_eq!(after("", &["3", "i", "a", "Escape"]), "a");
    let (e, _) = run("a\nb", &["y", "y", "2", "p"]);
    assert_eq!(e.text(), "a\na\na\nb");
}

#[test]
fn text_round_trips() {
    for t in ["", "a", "a\n", "\n", "a\n\nb", "  spaced  \nx"] {
        assert_eq!(Editor::with_text(t).text(), t);
    }
}
