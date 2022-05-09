use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use grep_matcher::{Match, Matcher, NoError};
use regex::bytes::{CaptureLocations, Regex};
use thread_local::ThreadLocal;

use crate::config::ConfiguredHIR;
use crate::error::Error;
use crate::matcher::RegexCaptures;

/// A matcher for implementing "word match" semantics.
#[derive(Debug)]
pub struct WordMatcher {
    /// The regex which is roughly `(?:^|\W)(<original pattern>)(?:$|\W)`.
    regex: Regex,
    /// The original regex supplied by the user, which we use in a fast path
    /// to try and detect matches before deferring to slower engines.
    original: Regex,
    /// A map from capture group name to capture group index.
    names: HashMap<String, usize>,
    /// A reusable buffer for finding the match location of the inner group.
    locs: Arc<ThreadLocal<RefCell<CaptureLocations>>>,
}

impl Clone for WordMatcher {
    fn clone(&self) -> WordMatcher {
        // We implement Clone manually so that we get a fresh ThreadLocal such
        // that it can set its own thread owner. This permits each thread
        // usings `locs` to hit the fast path.
        WordMatcher {
            regex: self.regex.clone(),
            original: self.original.clone(),
            names: self.names.clone(),
            locs: Arc::new(ThreadLocal::new()),
        }
    }
}

impl WordMatcher {
    /// Create a new matcher from the given pattern that only produces matches
    /// that are considered "words."
    ///
    /// The given options are used to construct the regular expression
    /// internally.
    pub fn new(expr: &ConfiguredHIR) -> Result<WordMatcher, Error> {
        let original =
            expr.with_pattern(|pat| format!("^(?:{})$", pat))?.regex()?;
        let word_expr = expr.with_pattern(|pat| {
            let pat = format!(r"(?:(?m:^)|\W)({})(?:\W|(?m:$))", pat);
            log::debug!("word regex: {:?}", pat);
            pat
        })?;
        let regex = word_expr.regex()?;
        let locs = Arc::new(ThreadLocal::new());

        let mut names = HashMap::new();
        for (i, optional_name) in regex.capture_names().enumerate() {
            if let Some(name) = optional_name {
                names.insert(name.to_string(), i.checked_sub(1).unwrap());
            }
        }
        Ok(WordMatcher { regex, original, names, locs })
    }

    /// Return the underlying regex used by this matcher.
    pub fn regex(&self) -> &Regex {
        &self.regex
    }

    /// Attempt to do a fast confirmation of a word match that covers a subset
    /// (but hopefully a big subset) of most cases. Ok(Some(..)) is returned
    /// when a match is found. Ok(None) is returned when there is definitively
    /// no match. Err(()) is returned when this routine could not detect
    /// whether there was a match or not.
    fn fast_find(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<Option<Match>, ()> {
        // This is a bit hairy. The whole point here is to avoid running an
        // NFA simulation in the regex engine. Remember, our word regex looks
        // like this:
        //
        //     (^|\W)(<original regex>)($|\W)
        //     where ^ and $ have multiline mode DISABLED
        //
        // What we want are the match offsets of <original regex>. So in the
        // easy/common case, the original regex will be sandwiched between
        // two codepoints that are in the \W class. So our approach here is to
        // look for a match of the overall word regexp, strip the \W ends and
        // then check whether the original regex matches what's left. If so,
        // then we are guaranteed a correct match.
        //
        // This only works though if we know that the match is sandwiched
        // between two \W codepoints. This only occurs when neither ^ nor $
        // match. This in turn only occurs when the match is at either the
        // beginning or end of the haystack. In either of those cases, we
        // declare defeat and defer to the slower implementation.
        //
        // The reason why we cannot handle the ^/$ cases here is because we
        // can't assume anything about the original pattern. (Try commenting
        // out the checks for ^/$ below and run the tests to see examples.)
        let mut cand = match self.regex.find_at(haystack, at) {
            None => return Ok(None),
            Some(m) => Match::new(m.start(), m.end()),
        };
        if cand.start() == 0 || cand.end() == haystack.len() {
            return Err(());
        }
        let (_, slen) = bstr::decode_utf8(&haystack[cand]);
        let (_, elen) = bstr::decode_last_utf8(&haystack[cand]);
        let new_start = cand.start() + slen;
        let new_end = cand.end() - elen;
        // This occurs the original regex can match the empty string. In this
        // case, just bail instead of trying to get it right here since it's
        // likely a pathological case.
        if new_start > new_end {
            return Err(());
        }
        cand = cand.with_start(new_start).with_end(new_end);
        if self.original.is_match(&haystack[cand]) {
            Ok(Some(cand))
        } else {
            Err(())
        }
    }
}

impl Matcher for WordMatcher {
    type Captures = RegexCaptures;
    type Error = NoError;

    fn find_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<Option<Match>, NoError> {
        // To make this easy to get right, we extract captures here instead of
        // calling `find_at`. The actual match is at capture group `1` instead
        // of `0`. We *could* use `find_at` here and then trim the match after
        // the fact, but that's a bit harder to get right, and it's not clear
        // if it's worth it.
        //
        // OK, well, it turns out that it is worth it! But it is quite tricky.
        // See `fast_find` for details. Effectively, this lets us skip running
        // the NFA simulation in the regex engine in the vast majority of
        // cases. However, the NFA simulation is required for full correctness.
        match self.fast_find(haystack, at) {
            Ok(Some(m)) => return Ok(Some(m)),
            Ok(None) => return Ok(None),
            Err(()) => {}
        }

        let cell =
            self.locs.get_or(|| RefCell::new(self.regex.capture_locations()));
        let mut caps = cell.borrow_mut();
        self.regex.captures_read_at(&mut caps, haystack, at);
        Ok(caps.get(1).map(|m| Match::new(m.0, m.1)))
    }

    fn new_captures(&self) -> Result<RegexCaptures, NoError> {
        Ok(RegexCaptures::with_offset(self.regex.capture_locations(), 1))
    }

    fn capture_count(&self) -> usize {
        self.regex.captures_len().checked_sub(1).unwrap()
    }

    fn capture_index(&self, name: &str) -> Option<usize> {
        self.names.get(name).map(|i| *i)
    }

    fn captures_at(
        &self,
        haystack: &[u8],
        at: usize,
        caps: &mut RegexCaptures,
    ) -> Result<bool, NoError> {
        let r =
            self.regex.captures_read_at(caps.locations_mut(), haystack, at);
        Ok(r.is_some())
    }

    // We specifically do not implement other methods like find_iter or
    // captures_iter. Namely, the iter methods are guaranteed to be correct
    // by virtue of implementing find_at and captures_at above.
}

#[cfg(test)]
mod tests {
    use super::WordMatcher;
    use crate::config::Config;
    use grep_matcher::{Captures, Match, Matcher};

    fn matcher(pattern: &str) -> WordMatcher {
        let chir = Config::default().hir(pattern).unwrap();
        WordMatcher::new(&chir).unwrap()
    }

    fn find(pattern: &str, haystack: &str) -> Option<(usize, usize)> {
        matcher(pattern)
            .find(haystack.as_bytes())
            .unwrap()
            .map(|m| (m.start(), m.end()))
    }

    fn find_by_caps(pattern: &str, haystack: &str) -> Option<(usize, usize)> {
        let m = matcher(pattern);
        let mut caps = m.new_captures().unwrap();
        if !m.captures(haystack.as_bytes(), &mut caps).unwrap() {
            None
        } else {
            caps.get(0).map(|m| (m.start(), m.end()))
        }
    }

    // Test that the standard `find` API reports offsets correctly.
    #[test]
    fn various_find() {
        assert_eq!(Some((0, 3)), find(r"foo", "foo"));
        assert_eq!(Some((0, 3)), find(r"foo", "foo("));
        assert_eq!(Some((1, 4)), find(r"foo", "!foo("));
        assert_eq!(None, find(r"foo", "!afoo("));

        assert_eq!(Some((0, 3)), find(r"foo", "foo☃"));
        assert_eq!(None, find(r"foo", "fooб"));

        assert_eq!(Some((0, 4)), find(r"foo5", "foo5"));
        assert_eq!(None, find(r"foo", "foo5"));

        assert_eq!(Some((1, 4)), find(r"foo", "!foo!"));
        assert_eq!(Some((1, 5)), find(r"foo!", "!foo!"));
        assert_eq!(Some((0, 5)), find(r"!foo!", "!foo!"));

        assert_eq!(Some((0, 3)), find(r"foo", "foo\n"));
        assert_eq!(Some((1, 4)), find(r"foo", "!foo!\n"));
        assert_eq!(Some((1, 5)), find(r"foo!", "!foo!\n"));
        assert_eq!(Some((0, 5)), find(r"!foo!", "!foo!\n"));

        assert_eq!(Some((1, 6)), find(r"!?foo!?", "!!foo!!"));
        assert_eq!(Some((0, 5)), find(r"!?foo!?", "!foo!"));
        assert_eq!(Some((2, 5)), find(r"!?foo!?", "a!foo!a"));

        assert_eq!(Some((2, 7)), find(r"!?foo!?", "##!foo!\n"));
        assert_eq!(Some((3, 8)), find(r"!?foo!?", "##\n!foo!##"));
        assert_eq!(Some((3, 8)), find(r"!?foo!?", "##\n!foo!\n##"));
        assert_eq!(Some((3, 7)), find(r"f?oo!?", "##\nfoo!##"));
        assert_eq!(Some((2, 5)), find(r"(?-u)foo[^a]*", "#!foo☃aaa"));
    }

    // See: https://github.com/BurntSushi/ripgrep/issues/389
    #[test]
    fn regression_dash() {
        assert_eq!(Some((0, 2)), find(r"-2", "-2"));
    }

    // Test that the captures API also reports offsets correctly, just as
    // find does. This exercises a different path in the code since captures
    // are handled differently.
    #[test]
    fn various_captures() {
        assert_eq!(Some((0, 3)), find_by_caps(r"foo", "foo"));
        assert_eq!(Some((0, 3)), find_by_caps(r"foo", "foo("));
        assert_eq!(Some((1, 4)), find_by_caps(r"foo", "!foo("));
        assert_eq!(None, find_by_caps(r"foo", "!afoo("));

        assert_eq!(Some((0, 3)), find_by_caps(r"foo", "foo☃"));
        assert_eq!(None, find_by_caps(r"foo", "fooб"));
        // assert_eq!(Some((0, 3)), find_by_caps(r"foo", "fooб"));

        // See: https://github.com/BurntSushi/ripgrep/issues/389
        assert_eq!(Some((0, 2)), find_by_caps(r"-2", "-2"));
    }

    // Test that the capture reporting methods work as advertised.
    #[test]
    fn capture_indexing() {
        let m = matcher(r"(a)(?P<foo>b)(c)");
        assert_eq!(4, m.capture_count());
        assert_eq!(Some(2), m.capture_index("foo"));

        let mut caps = m.new_captures().unwrap();
        assert_eq!(4, caps.len());

        assert!(m.captures(b"abc", &mut caps).unwrap());
        assert_eq!(caps.get(0), Some(Match::new(0, 3)));
        assert_eq!(caps.get(1), Some(Match::new(0, 1)));
        assert_eq!(caps.get(2), Some(Match::new(1, 2)));
        assert_eq!(caps.get(3), Some(Match::new(2, 3)));
        assert_eq!(caps.get(4), None);

        assert!(m.captures(b"#abc#", &mut caps).unwrap());
        assert_eq!(caps.get(0), Some(Match::new(1, 4)));
        assert_eq!(caps.get(1), Some(Match::new(1, 2)));
        assert_eq!(caps.get(2), Some(Match::new(2, 3)));
        assert_eq!(caps.get(3), Some(Match::new(3, 4)));
        assert_eq!(caps.get(4), None);
    }
}
