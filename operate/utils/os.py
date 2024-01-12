import pyautogui
import platform
import time
import math

from operate.utils.misc import convert_percent_to_decimal


def keyboard(content):
    """
    Types the given text using the keyboard.

    Args:
        text (str): The text to be typed.

    Returns:
        str: A message indicating the typed text.
    """
    content = content.replace("\\n", "\n")
    for char in content:
        pyautogui.write(char)

    return "Type: " + content


def press(keys):
    print("[hotkey] ")
    print("[hotkey] keys", keys)
    for key in keys:
        print("[hotkey] keydown", key)
        pyautogui.keyDown(key)
    time.sleep(0.1)
    for key in keys:
        print("[hotkey] keyup", key)
        pyautogui.keyUp(key)
    return True


def mouse(click_detail):
    """
    Perform a mouse click at the specified coordinates.

    Args:
        click_detail (dict): A dictionary containing the coordinates of the click.

    Returns:
        str: The description of the click if successful, otherwise "We failed to click".
    """
    try:
        x = convert_percent_to_decimal(click_detail["x"])
        y = convert_percent_to_decimal(click_detail["y"])

        if click_detail and isinstance(x, float) and isinstance(y, float):
            click_at_percentage(x, y)
            return "we clicked successfully"
        else:
            return "We failed to click"

    except Exception as e:
        print(f"Error parsing JSON: {e}")
        return "We failed to click"


def click_at_percentage(
    x_percentage, y_percentage, duration=0.2, circle_radius=50, circle_duration=0.5
):
    """
    Moves the mouse cursor to a specified percentage of the screen and performs a circular movement before clicking.

    Args:
        x_percentage (float): The x-coordinate percentage of the screen to move the cursor to.
        y_percentage (float): The y-coordinate percentage of the screen to move the cursor to.
        duration (float, optional): The duration (in seconds) of the smooth cursor movement. Defaults to 0.2.
        circle_radius (int, optional): The radius of the circular movement. Defaults to 50.
        circle_duration (float, optional): The duration (in seconds) of the circular movement. Defaults to 0.5.

    Returns:
        str: A message indicating that the click was successful.
    """
    # Get the size of the primary monitor
    screen_width, screen_height = pyautogui.size()

    # Calculate the x and y coordinates in pixels
    x_pixel = int(screen_width * float(x_percentage))
    y_pixel = int(screen_height * float(y_percentage))

    # Move to the position smoothly
    pyautogui.moveTo(x_pixel, y_pixel, duration=duration)

    # Circular movement
    start_time = time.time()
    while time.time() - start_time < circle_duration:
        angle = ((time.time() - start_time) / circle_duration) * 2 * math.pi
        x = x_pixel + math.cos(angle) * circle_radius
        y = y_pixel + math.sin(angle) * circle_radius
        pyautogui.moveTo(x, y, duration=0.1)

    # Finally, click
    pyautogui.click(x_pixel, y_pixel)
    return "Successfully clicked"


def get_last_assistant_message(messages):
    """
    Retrieve the last message from the assistant in the messages array.
    If the last assistant message is the first message in the array, return None.
    """
    for index in reversed(range(len(messages))):
        if messages[index]["role"] == "assistant":
            if index == 0:  # Check if the assistant message is the first in the array
                return None
            else:
                return messages[index]
    return None  # Return None if no assistant message is found
