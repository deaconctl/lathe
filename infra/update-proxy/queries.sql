-- name: daily
SELECT toStartOfDay(timestamp) AS day, count(DISTINCT index1) AS active_installs
FROM lathe_pings
WHERE timestamp >= NOW() - INTERVAL '30' DAY
GROUP BY day
ORDER BY day;

-- name: weekly
SELECT toStartOfWeek(timestamp) AS week, count(DISTINCT index1) AS active_installs
FROM lathe_pings
WHERE timestamp >= NOW() - INTERVAL '30' DAY
GROUP BY week
ORDER BY week;

-- name: versions
SELECT blob1 AS version, count(DISTINCT index1) AS active_installs
FROM lathe_pings
WHERE timestamp >= NOW() - INTERVAL '7' DAY
GROUP BY version
ORDER BY active_installs DESC;

-- name: channels
SELECT blob2 AS channel, count(DISTINCT index1) AS active_installs
FROM lathe_pings
WHERE timestamp >= NOW() - INTERVAL '7' DAY
GROUP BY channel
ORDER BY active_installs DESC;

-- name: platforms
SELECT blob3 AS os, blob4 AS arch, count(DISTINCT index1) AS active_installs
FROM lathe_pings
WHERE timestamp >= NOW() - INTERVAL '7' DAY
GROUP BY os, arch
ORDER BY active_installs DESC;

-- name: sampling
SELECT max(_sample_interval) AS largest_sample_interval
FROM lathe_pings
WHERE timestamp >= NOW() - INTERVAL '30' DAY;
