export const MUSE_WEB_BUSY = "MUSE_WEB_BUSY";
export const MUSE_QUOTA_BADGE = "Out of tokens · Auto-disabled";
export const MUSE_QUOTA_REASON = "quota_exhausted";
export const SESSION_EXPIRED_BADGE = "Session hết hiệu lực · tự động tắt";
export const SESSION_EXPIRED_REASON = "session_expired";

export const MUSE_WEB_TIMEOUTS = {
  sessionReuse: 60000,
  pageReady: 45000,
  pollMin: 120,
  pollMax: 600,
  textStable: 600,
  stall: 15000,
  firstToken: 45000,
  chat: 300000,
  image: 240000,
  video: 600000,
};
