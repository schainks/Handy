import React, { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { commands, type VoiceCommandsStatus } from "@/bindings";
import { SettingContainer, SettingsGroup, Slider } from "@/components/ui";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { Input } from "../../ui/Input";
import { ApiKeyField } from "../PostProcessingSettingsApi/ApiKeyField";
import { useSettings } from "../../../hooks/useSettings";

const ModelField: React.FC<{
  value: string;
  onBlur: (value: string) => void;
  disabled: boolean;
}> = ({ value, onBlur, disabled }) => {
  const [localValue, setLocalValue] = useState(value);

  useEffect(() => {
    setLocalValue(value);
  }, [value]);

  return (
    <Input
      type="text"
      value={localValue}
      onChange={(event) => setLocalValue(event.target.value)}
      onBlur={() => onBlur(localValue)}
      variant="compact"
      disabled={disabled}
      className="min-w-[200px]"
    />
  );
};

export const VoiceCommandsSettings: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, resetSetting, isUpdating } = useSettings();
  const [status, setStatus] = useState<VoiceCommandsStatus | null>(null);

  const refreshStatus = useCallback(async () => {
    try {
      const result = await commands.getVoiceCommandsStatus();
      if (result.status === "ok") {
        setStatus(result.data);
      }
    } catch (error) {
      console.error("Failed to load voice command status:", error);
    }
  }, []);

  // The custom commands file is edited in another app, so re-read the status
  // whenever the settings window regains focus.
  useEffect(() => {
    refreshStatus();
    window.addEventListener("focus", refreshStatus);
    return () => window.removeEventListener("focus", refreshStatus);
  }, [refreshStatus]);

  const apiKey = getSetting("voice_commands_api_key") ?? "";
  const model = getSetting("voice_commands_model") ?? "";
  const threshold = getSetting("voice_commands_threshold") ?? 0.7;

  const openCustomCommands = async () => {
    const result = await commands.openVoiceCommandsFile();
    if (result.status === "error") {
      toast.error(result.error);
    }
    refreshStatus();
  };

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      {status && !status.supported && (
        <Alert variant="warning">
          {t("settings.voiceCommands.unsupported")}
        </Alert>
      )}

      <SettingsGroup title={t("settings.voiceCommands.groups.jev")}>
        <SettingContainer
          title={t("settings.voiceCommands.apiKey.title")}
          description={t("settings.voiceCommands.apiKey.description")}
          descriptionMode="tooltip"
          layout="horizontal"
          grouped={true}
        >
          <ApiKeyField
            value={apiKey}
            onBlur={(value) => updateSetting("voice_commands_api_key", value)}
            placeholder={t("settings.voiceCommands.apiKey.placeholder")}
            disabled={isUpdating("voice_commands_api_key")}
            className="min-w-[320px]"
          />
        </SettingContainer>
        {!apiKey.trim() && (
          <Alert variant="info" contained>
            {t("settings.voiceCommands.apiKey.missing")}
          </Alert>
        )}

        <SettingContainer
          title={t("settings.voiceCommands.model.title")}
          description={t("settings.voiceCommands.model.description")}
          descriptionMode="tooltip"
          layout="horizontal"
          grouped={true}
        >
          <ModelField
            value={model}
            onBlur={(value) => updateSetting("voice_commands_model", value)}
            disabled={isUpdating("voice_commands_model")}
          />
        </SettingContainer>

        <Slider
          value={threshold}
          onChange={(value) => updateSetting("voice_commands_threshold", value)}
          onReset={() => resetSetting("voice_commands_threshold")}
          isResetting={isUpdating("voice_commands_threshold")}
          min={0.5}
          max={0.95}
          step={0.05}
          label={t("settings.voiceCommands.threshold.title")}
          description={t("settings.voiceCommands.threshold.description")}
          descriptionMode="tooltip"
          grouped={true}
        />
      </SettingsGroup>

      <SettingsGroup title={t("settings.voiceCommands.groups.actions")}>
        <SettingContainer
          title={t("settings.voiceCommands.hammerspoon.title")}
          description={t("settings.voiceCommands.hammerspoon.description")}
          descriptionMode="tooltip"
          layout="horizontal"
          grouped={true}
        >
          <span className="text-sm">
            {status?.hammerspoon
              ? t("settings.voiceCommands.hammerspoon.installed")
              : t("settings.voiceCommands.hammerspoon.notInstalled")}
          </span>
        </SettingContainer>
        {status && !status.hammerspoon && (
          <Alert variant="info" contained>
            {t("settings.voiceCommands.hammerspoon.setup")}
          </Alert>
        )}

        <SettingContainer
          title={t("settings.voiceCommands.customCommands.title")}
          description={t("settings.voiceCommands.customCommands.description")}
          descriptionMode="tooltip"
          layout="horizontal"
          grouped={true}
        >
          <div className="flex items-center gap-3">
            {status && (
              <span
                className="text-sm text-mid-gray"
                title={status.custom_commands_path}
              >
                {t("settings.voiceCommands.customCommands.count", {
                  builtin: status.builtin_actions,
                  custom: status.custom_commands,
                })}
              </span>
            )}
            <Button variant="secondary" size="sm" onClick={openCustomCommands}>
              {t("settings.voiceCommands.customCommands.edit")}
            </Button>
          </div>
        </SettingContainer>
        {status?.custom_commands_error && (
          <Alert variant="error" contained>
            {t("settings.voiceCommands.customCommands.error", {
              error: status.custom_commands_error,
            })}
          </Alert>
        )}
      </SettingsGroup>
    </div>
  );
};
