import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  basePath: "/sem",
  eslint: {
    ignoreDuringBuilds: true,
  },
};

export default nextConfig;
