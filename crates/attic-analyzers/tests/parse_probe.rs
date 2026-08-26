//! Phase 3 probe: dump ACTUAL parse-tree node kinds for seed fixtures.
//! Run manually: cargo test -p attic-analyzers --test parse_probe -- --nocapture
//! This test exists to ground the canonical mapping in reality; it is
//! #[ignore]d by default so CI never depends on its output.

use tree_sitter::Parser;

fn dump(lang: &str, language: tree_sitter::Language, code: &str) {
    let mut parser = Parser::new();
    parser.set_language(&language).expect("set_language");
    let tree = parser.parse(code, None).expect("parse");
    let mut out = String::new();
    fn walk(node: tree_sitter::Node, src: &str, depth: usize, out: &mut String) {
        if depth > 6 {
            return;
        }
        let kind = node.kind();
        let field_note = if node.is_named() { "" } else { "(anon)" };
        let text: String = {
            let b = node.byte_range();
            let s = &src[b.start..b.end.min(src.len())];
            s.chars().take(40).collect::<String>().replace('\n', "\\n")
        };
        out.push_str(&format!(
            "{}{} {} {:?}\n",
            "  ".repeat(depth),
            kind,
            field_note,
            text
        ));
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            walk(ch, src, depth + 1, out);
        }
    }
    walk(tree.root_node(), code, 0, &mut out);
    println!("===== {} =====\n{}", lang.to_uppercase(), out);
}

const JAVA: &str = r#"package com.example.app;

import java.util.List;
import com.example.lib.Helper;
import static java.lang.Math.max;

public class FooService extends Base implements AutoCloseable {
    private static final int LIMIT = 10;
    private List<String> items;

    public FooService(List<String> items) { this.items = items; }

    @Override
    public void close() throws Exception {
        items.clear();
    }

    protected int compute(int a, int b) throws IllegalStateException {
        return max(a, b) + LIMIT;
    }
}
"#;

const PYTHON: &str = r##""""Module docstring."""
import os
import os.path as osp
from collections import OrderedDict
from . import sibling
from ..pkg.sub import thing as alias
from .relmod import *

@dataclass(frozen=True)
class Base(BaseProto, metaclass=Meta):
    """Base doc."""

    attr: int = 3

    def __init__(self, x):
        self.x = x

    @property
    def prop(self):
        return self.x

    async def fetch(self, url):
        data = await get(url)
        return data

def top_level(a, b=2, *args, **kw):
    def nested():
        return a
    return nested()

MAX_K = 10
lambda_fn = lambda q: q + 1
"##;

const GO: &str = r#"package server

import (
	"fmt"
	"strings"

	"github.com/example/repo/internal/util"
)

const MaxSize = 100

type Handler struct {
	Name string
	Size int
}

type Router interface {
	Route(path string) error
	String() string
}

func NewHandler(name string) *Handler {
	return &Handler{Name: name}
}

func (h *Handler) Route(path string) error {
	if strings.HasPrefix(path, "/api") {
		return fmt.Errorf("bad: %s", path)
	}
	return nil
}
"#;

const JS: &str = r#"import React, { useState } from 'react';
import { helper } from "./util.js";
const legacy = require("./legacy.cjs");
export * from "./reexport.js";

export const VERSION = "1.0";

export class Widget extends Base {
    #priv = 1;
    render(props) {
        return helper(props);
    }
}

export function makeWidget(opts) {
    function inner() { return opts.id; }
    return new Widget(inner());
}

const arrow = (a, b) => a + b;
async function load() { return await fetch("/x"); }
module.exports = { makeWidget };
"#;

const TS: &str = r#"import type { Config } from "./config";
import { Component, Vue } from "vue";
export interface Options { id: string; size?: number }
export type Alias = Options | null;

export abstract class BaseWidget<T> extends Vue implements Component<T> {
    private cached?: T;
    constructor(readonly cfg: Config) { super(); }
    abstract render(): string;
    get value(): T | undefined { return this.cached; }
}

export namespace Util {
    export function clamp(v: number): number { return v; }
}

export default function build(o: Options): BaseWidget<string> {
    return null as any;
}
enum Color { Red = 1 }
"#;

#[test]
#[ignore]
fn probe_parse_trees() {
    dump("java", tree_sitter_java::LANGUAGE.into(), JAVA);
    dump("python", tree_sitter_python::LANGUAGE.into(), PYTHON);
}

#[test]
#[ignore]
fn probe_parse_trees_go_js_ts() {
    dump("go", tree_sitter_go::LANGUAGE.into(), GO);
    dump("javascript", tree_sitter_javascript::LANGUAGE.into(), JS);
    dump(
        "typescript",
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        TS,
    );
}
