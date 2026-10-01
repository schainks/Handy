import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface VoiceCommandsToggleProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const VoiceCommandsToggle: React.FC<VoiceCommandsToggleProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const enabled = getSetting("voice_commands_enabled") || false;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(enabled) => updateSetting("voice_commands_enabled", enabled)}
        isUpdating={isUpdating("voice_commands_enabled")}
        label={t("settings.advanced.voiceCommandsToggle.label")}
        description={t("settings.advanced.voiceCommandsToggle.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  });
