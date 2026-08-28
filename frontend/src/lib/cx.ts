/**
 * Joins CSS module class names, dropping falsy ones.
 *
 * Exists because `noUncheckedIndexedAccess` types every CSS-module class
 * lookup (`styles.someClass`) as `string | undefined` — a real guard
 * against typo'd class names, but it also makes `undefined` illegal as a
 * `classList` object's computed key. Building the class string instead
 * sidesteps that without a type assertion at every call site.
 */
export function cx(...classNames: Array<string | false | null | undefined>): string {
  return classNames.filter((name): name is string => Boolean(name)).join(" ");
}
