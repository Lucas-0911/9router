// Reuse the compiler already shipped by Next; no server runtime is bundled.
module.exports = function (source) {
  const callback = this.async();
  require("next/dist/build/swc").transform(source, {
    filename: this.resourcePath,
    jsc: { parser: { syntax: "ecmascript", jsx: true, importAttributes: true }, target: "es2020", transform: { react: { runtime: "automatic" } } },
    module: { type: "es6" },
  }).then(({ code }) => callback(null, code), callback);
};
