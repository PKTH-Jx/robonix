# SPDX-License-Identifier: MulanPSL-2.0
"""Sample the MuJoCo bridge's ROS 2 odometry without starting a viewer."""

import json
import math
import time

import rclpy
from nav_msgs.msg import Odometry

rclpy.init()
node = rclpy.create_node("srdl_test_odometry")


def save(msg):
    point = msg.pose.pose.position
    twist = msg.twist.twist
    print(json.dumps({
        "time": time.monotonic(), "x": point.x, "y": point.y,
        "speed": math.hypot(twist.linear.x, twist.linear.y),
        "yaw_rate": twist.angular.z,
    }), flush=True)


node.create_subscription(Odometry, "/odom", save, 10)
try:
    rclpy.spin(node)
finally:
    node.destroy_node()
    rclpy.try_shutdown()
