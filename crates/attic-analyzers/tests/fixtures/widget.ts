/**
 * TS fixture. Code-like text in comments: interface NotReal { id: string }
 */
import type { Config } from './config';
import { Component } from 'vue';

export interface Options {
  id: string;
  size?: number;
  run?(): void;
}

export type Maybe<T> = T | null;

export enum Color { Red = 1, Green }

export namespace Util {
  export function clamp(v: number): number { return v; }
}

export abstract class BaseWidget<T> extends Vue implements Component<Config> {
  private cached?: T;

  constructor(readonly cfg: Config) { super(); }

  abstract render(): string;

  get value(): T | undefined { return this.cached; }
}

export default function build(o: Options): BaseWidget<string> {
  return null as unknown as BaseWidget<string>;
}
