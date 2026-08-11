/**
 * Reference profile для тестов движка.
 *
 * Регрессионный набор писался на реальном репозитории (Orakul): раскладка `app/`, слои
 * server/shared/client, маркеры `orakul-ai-review`. Он же — доказательство, что вынос движка
 * в отдельный пакет ничего не поменял: те же входы дают те же выводы под тем же профилем.
 * Поведение дефолтного (нейтрального) профиля проверяет test/profile.test.mjs.
 */

import { fileURLToPath } from 'node:url';
import { loadProfileFile, setProfile } from '../../lib/profile.mjs';

export const REFERENCE_PROFILE_PATH = fileURLToPath(new URL('../../profiles/orakul.json', import.meta.url));
export const referenceProfile = setProfile(loadProfileFile(REFERENCE_PROFILE_PATH));
