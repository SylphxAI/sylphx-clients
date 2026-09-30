// `cacheHandler: require.resolve('@sylphx/next/cache-handler')` in next.config.
import { createCacheHandler } from './isr.js'

export default createCacheHandler()
