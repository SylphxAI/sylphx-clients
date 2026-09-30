// `cacheHandlers: { default: require.resolve('@sylphx/next/use-cache-handler') }` in next.config.
import { createUseCacheHandler } from './use-cache.js'

export default createUseCacheHandler()
