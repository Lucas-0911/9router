import { lazy, Suspense } from "react";
export default function dynamic(loader, options = {}) {
  const Component = lazy(async () => {
    const module = await loader();
    return { default: module.default || module };
  });
  return function Dynamic(props) {
    return <Suspense fallback={options.loading ? <options.loading /> : null}><Component {...props} /></Suspense>;
  };
}
