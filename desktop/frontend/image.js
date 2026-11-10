import { forwardRef } from "react";
export default forwardRef(function Image({ src, alt, width, height, fill, priority, quality, unoptimized, placeholder, blurDataURL, loader, onLoadingComplete, style, ...props }, ref) {
  return <img {...props} ref={ref} src={typeof src === "object" ? src.src : src} alt={alt} width={width} height={height} loading={priority ? "eager" : "lazy"} style={fill ? { position: "absolute", height: "100%", width: "100%", inset: 0, ...style } : style} />;
});
