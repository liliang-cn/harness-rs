import i18n from 'i18next';
import LanguageDetector from 'i18next-browser-languagedetector';
import { initReactI18next } from 'react-i18next';

import en from '@/locales/en.json';
import zh from '@/locales/zh.json';

// Detect order: explicit choice in localStorage → ?lng=zh-CN → browser
// language. Browser default "zh-*" maps to `zh` thanks to nonExplicitSupportedLngs.
i18n
  .use(LanguageDetector)
  .use(initReactI18next)
  .init({
    resources: {
      en: { translation: en },
      zh: { translation: zh },
    },
    fallbackLng: 'en',
    nonExplicitSupportedLngs: true, // zh-CN / zh-Hans / zh-TW all match "zh"
    supportedLngs: ['en', 'zh'],
    interpolation: { escapeValue: false }, // React already escapes
    detection: {
      order: ['localStorage', 'querystring', 'navigator'],
      caches: ['localStorage'],
      lookupLocalStorage: 'ledger-lang',
    },
  });

export default i18n;
