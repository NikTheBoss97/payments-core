-- Author: Nicola Antonello

-- An endpoint's URL names a host, and a port that a client reads as written (ADR-0061):
-- an IP literal or a name, then nothing, or a colon and decimal digits whose value is at
-- most 65535, none meaning port 80. The rest is as before (ADR-0055). The regular
-- expression gives the shape, and the comparison the port's range. Adding the check
-- checks every stored row.
ALTER TABLE webhook_endpoints
    DROP CONSTRAINT webhook_endpoints_url_check,
    ADD CONSTRAINT webhook_endpoints_url_check CHECK (
        url ~ '^http://(\[[^]/?#@[:space:][:cntrl:]]+\]|[^]/?#@:[[:space:][:cntrl:]]+)(:[0-9]*)?([/?][^#[:space:][:cntrl:]]*)?$'
        AND COALESCE(NULLIF(substring(url FROM '^http://(?:\[[^]]*\]|[^]/?#@:[]*):([0-9]*)'), ''), '0')::numeric <= 65535
    );
