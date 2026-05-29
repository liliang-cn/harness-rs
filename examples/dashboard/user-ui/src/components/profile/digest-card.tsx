import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Label } from '@/components/ui/label';
import { Skeleton } from '@/components/ui/skeleton';
import { ledgerApi, type DigestSettings } from '@/lib/api';

const TIMEZONES = [
  'Asia/Shanghai',
  'Asia/Hong_Kong',
  'Asia/Tokyo',
  'Asia/Singapore',
  'America/New_York',
  'America/Los_Angeles',
  'Europe/London',
  'Europe/Paris',
  'UTC',
];

export function DigestCard() {
  const { t } = useTranslation();
  const [loaded, setLoaded] = useState(false);
  const [enabled, setEnabled] = useState(false);
  const [time, setTime] = useState('08:00');
  const [timezone, setTimezone] = useState('Asia/Shanghai');
  const [channel, setChannel] = useState<DigestSettings['channel']>('in_app');

  useEffect(() => {
    let cancelled = false;
    ledgerApi
      .digestSettings()
      .then((r) => {
        if (cancelled) return;
        setEnabled(r.settings.enabled);
        setTime(r.settings.send_time);
        setTimezone(r.settings.timezone);
        setChannel(r.settings.channel);
        setLoaded(true);
      })
      .catch(() => {
        if (!cancelled) setLoaded(true);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  async function save(patch: {
    enabled: boolean;
    time: string;
    timezone: string;
    channel: string;
  }) {
    try {
      await ledgerApi.saveDigestSettings(patch);
      toast.success(t('digest.saved'));
    } catch {
      toast.error(t('digest.saveFailed'));
    }
  }

  function handleEnabledChange(checked: boolean) {
    setEnabled(checked);
    save({ enabled: checked, time, timezone, channel });
  }

  function handleTimezoneChange(v: string) {
    setTimezone(v);
    save({ enabled, time, timezone: v, channel });
  }

  function handleChannelChange(v: string) {
    const c = v as DigestSettings['channel'];
    setChannel(c);
    save({ enabled, time, timezone, channel: v });
  }

  function handleTimeBlur(e: React.FocusEvent<HTMLInputElement>) {
    const v = e.target.value;
    setTime(v);
    save({ enabled, time: v, timezone, channel });
  }

  if (!loaded) {
    return (
      <Card>
        <CardHeader>
          <Skeleton className="h-5 w-32" />
        </CardHeader>
        <CardContent>
          <Skeleton className="h-9 w-full" />
        </CardContent>
      </Card>
    );
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{t('digest.title')}</CardTitle>
        <CardDescription>{t('digest.subtitle')}</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {/* Enable toggle */}
        <div className="flex items-center gap-2">
          <input
            id="digest-enabled"
            type="checkbox"
            checked={enabled}
            onChange={(e) => handleEnabledChange(e.target.checked)}
            className="h-4 w-4 rounded border-gray-300 text-primary accent-primary cursor-pointer"
          />
          <Label htmlFor="digest-enabled" className="cursor-pointer">
            {t('digest.enable')}
          </Label>
        </div>

        {enabled && (
          <div className="space-y-3 pl-1">
            {/* Send time */}
            <div className="space-y-1">
              <Label htmlFor="digest-time" className="text-sm">
                {t('digest.time')}
              </Label>
              <input
                id="digest-time"
                type="time"
                defaultValue={time}
                onBlur={handleTimeBlur}
                className="flex h-9 w-full rounded-md border border-input bg-background px-3 py-1 text-sm shadow-sm transition-colors focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring"
              />
            </div>

            {/* Timezone */}
            <div className="space-y-1">
              <Label className="text-sm">{t('digest.timezone')}</Label>
              <Select value={timezone} onValueChange={handleTimezoneChange}>
                <SelectTrigger className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {TIMEZONES.map((tz) => (
                    <SelectItem key={tz} value={tz}>
                      {tz}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>

            {/* Channel */}
            <div className="space-y-1">
              <Label className="text-sm">{t('digest.channel')}</Label>
              <Select value={channel} onValueChange={handleChannelChange}>
                <SelectTrigger className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="in_app">{t('digest.channelInApp')}</SelectItem>
                  <SelectItem value="email">{t('digest.channelEmail')}</SelectItem>
                  <SelectItem value="both">{t('digest.channelBoth')}</SelectItem>
                </SelectContent>
              </Select>
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  );
}
